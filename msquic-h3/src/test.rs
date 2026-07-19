use std::sync::Arc;

use bytes::Buf;
use h3::error::ConnectionError;
use http::Uri;
use msquic::{BufferRef, CredentialConfig, CredentialFlags, RegistrationConfig, Settings};

use crate::Connection;

pub mod util {
    use msquic::Credential;
    // used for debugging
    pub const DEVEL_TRACE_LEVEL: tracing::Level = tracing::Level::TRACE;

    pub fn try_setup_tracing() {
        let _ = tracing_subscriber::fmt()
            .with_max_level(DEVEL_TRACE_LEVEL)
            .try_init();
    }

    /// Use pwsh to get the test cert hash
    #[cfg(target_os = "windows")]
    pub fn get_test_cred() -> Credential {
        use msquic::CertificateHash;

        let output = std::process::Command::new("pwsh.exe")
            .args(["-Command", "Get-ChildItem Cert:\\CurrentUser\\My | Where-Object -Property FriendlyName -EQ -Value MsQuic-Test | Select-Object -ExpandProperty Thumbprint -First 1"]).
            output().expect("Failed to execute command");
        assert!(output.status.success());
        let mut s = String::from_utf8(output.stdout).unwrap();
        if s.ends_with('\n') {
            s.pop();
            if s.ends_with('\r') {
                s.pop();
            }
        };
        Credential::CertificateHash(CertificateHash::from_str(&s).unwrap())
    }

    /// Generate a test cert if not present using openssl cli.
    #[cfg(not(target_os = "windows"))]
    pub fn get_test_cred() -> Credential {
        use msquic::CertificateFile;

        // Serialize cert generation across parallel tests in the same
        // process. Without this, two tests racing on the shared cert dir
        // can delete/recreate it out from under each other's `openssl`
        // invocation, which then fails to spawn with NotFound.
        static CERT_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _lock = CERT_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        let cert_dir = std::env::temp_dir().join("msquic_h3_test_rs");
        let key = "key.pem";
        let cert = "cert.pem";
        let key_path = cert_dir.join(key);
        let cert_path = cert_dir.join(cert);
        if !key_path.exists() || !cert_path.exists() {
            // remove the dir
            let _ = std::fs::remove_dir_all(&cert_dir);
            std::fs::create_dir_all(&cert_dir).expect("cannot create cert dir");
            // generate test cert using openssl cli
            let output = std::process::Command::new("openssl")
                .args([
                    "req",
                    "-x509",
                    "-newkey",
                    "rsa:4096",
                    "-keyout",
                    "key.pem",
                    "-out",
                    "cert.pem",
                    "-sha256",
                    "-days",
                    "3650",
                    "-nodes",
                    "-subj",
                    "/CN=localhost",
                ])
                .current_dir(&cert_dir)
                .stderr(std::process::Stdio::inherit())
                .stdout(std::process::Stdio::inherit())
                .output()
                .expect("cannot generate cert");
            if !output.status.success() {
                panic!("generate cert failed");
            }
        }
        Credential::CertificateFile(CertificateFile::new(
            key_path.display().to_string(),
            cert_path.display().to_string(),
        ))
    }
}

pub(crate) async fn send_get_request(uri: Uri) {
    let app_name = String::from("testapp");
    let config = RegistrationConfig::new().set_app_name(app_name);
    let reg = Arc::new(crate::Registration::new(&config).unwrap());

    let alpn = BufferRef::from("h3");
    // create an client
    // open client
    let client_settings = Settings::new().set_IdleTimeoutMs(2000);
    let client_config = reg
        .open_configuration(&[alpn], Some(&client_settings))
        .unwrap();
    {
        let cred_config = CredentialConfig::new_client()
            .set_credential_flags(CredentialFlags::NO_CERTIFICATE_VALIDATION);
        client_config.load_credential(&cred_config).unwrap();
    }

    tracing::info!("client conn open and start");
    let conn = Connection::connect(
        &reg,
        &client_config,
        uri.host().unwrap(),
        uri.port_u16().unwrap(),
    )
    .await
    .unwrap();

    tracing::info!("client create h3 client");
    let (mut driver, mut send_request) = h3::client::new(conn).await.unwrap();

    tracing::info!("client start driver");
    let drive = async move {
        Err::<(), ConnectionError>(futures::future::poll_fn(|cx| driver.poll_close(cx)).await)
    };

    // tokio::time::sleep(std::time::Duration::from_millis(3)).await;
    // In the following block, we want to take ownership of `send_request`:
    // the connection will be closed only when all `SendRequest`s instances
    // are dropped.
    //
    //             So we "move" it.
    //                  vvvv
    let request = async move {
        tracing::info!("sending request ...");

        let req = http::Request::builder().uri(uri).body(())?;

        // sending request results in a bidirectional stream,
        // which is also used for receiving response
        let mut stream = send_request.send_request(req).await?;

        // finish on the sending side
        stream.finish().await?;

        tracing::info!("receiving response ...");

        let resp = stream.recv_response().await?;

        tracing::info!("response: {:?} {}", resp.version(), resp.status());
        tracing::info!("headers: {:#?}", resp.headers());

        // `recv_data()` must be called after `recv_response()` for
        // receiving potential response body
        let mut data = vec![];
        while let Some(mut chunk) = stream.recv_data().await? {
            // let mut out = tokio::io::stdout();
            // tokio::io::AsyncWriteExt::write_all_buf(&mut out, &mut chunk).await?;
            // tokio::io::AsyncWriteExt::flush(&mut out).await?;
            let mut dst = vec![0; chunk.remaining()];
            chunk.copy_to_slice(&mut dst[..]);
            data.extend_from_slice(&dst);
        }
        let body = String::from_utf8_lossy(&data);
        tracing::info!("client got body: {}", body);
        // tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        Ok::<_, Box<dyn std::error::Error>>(())
    };

    let (req_res, drive_res) = tokio::join!(request, drive);
    if let Err(e) = req_res {
        tracing::error!("req_err {e:?}");
    }
    if let Err(e) = drive_res {
        tracing::error!("drive_res {e:?}");
    }
    tracing::info!("client ended success");

    // Exercise the teardown contract: after the driver ended and the h3
    // client (owning the Connection) was dropped, shutdown + wait_idle must
    // resolve once every connection handle has closed. A timeout here would
    // signal the RegistrationClose-blocking hang this feature prevents.
    reg.shutdown();
    tokio::time::timeout(std::time::Duration::from_secs(5), reg.wait_idle())
        .await
        .expect("wait_idle should resolve after the connection closed");
}

/// Manual-only external smoke check (MF-3): drives a real HTTP/3 GET against a
/// public third-party endpoint (`h2o.examp1e.net`). It depends on DNS, remote
/// uptime, and ALPN/cert behavior, so it is `#[ignore]`d to keep the default
/// suite hermetic. Run it explicitly with
/// `cargo test --no-default-features --features native-find -- --ignored client_test_apache`
/// when networking is available. The loopback `conformance` suite covers the
/// client path hermetically.
#[test]
#[ignore = "requires external internet access (h2o.examp1e.net); run manually with --ignored"]
fn client_test_apache() {
    util::try_setup_tracing();
    // This does not work (cloudflare servers):
    // let uri = http::Uri::from_static("https://quic.tech:8443/");
    // let uri = http::Uri::from_static("https://cloudflare-quic.com:443/");

    // These works
    let uri = http::Uri::from_static("https://h2o.examp1e.net:443");
    // let uri = http::Uri::from_static("https://docs.trafficserver.apache.org:443/");
    // use tokio
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap()
        .block_on(send_get_request(uri));
}

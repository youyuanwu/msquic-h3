pub mod core;

pub mod msh3;

// Some useful defs
pub const QUIC_STATUS_PENDING: u32 = 0x703e5;
pub const QUIC_STATUS_SUCCESS: u32 = 0;

// macro for enabling tracing for internals.
macro_rules! trace {
    ($($rest:tt)*) => {
        #[cfg(feature = "trace")]
        tracing::trace!($($rest)*)
    }
}
pub(crate) use trace;

#[cfg(test)]
mod tests {

    use std::{process::Command, thread, time::Duration};

    use bytes::Bytes;
    use msquic_sys2::{
        Addr, CertificateHash, CertificateUnion, CredentialConfig, RegistrationConfig, Settings,
        ADDRESS_FAMILY_UNSPEC, CREDENTIAL_FLAG_CLIENT, CREDENTIAL_FLAG_NO_CERTIFICATE_VALIDATION,
        CREDENTIAL_TYPE_CERTIFICATE_HASH, CREDENTIAL_TYPE_NONE, EXECUTION_PROFILE_LOW_LATENCY,
        SEND_FLAG_FIN, STREAM_OPEN_FLAG_NONE, STREAM_START_FLAG_NONE,
    };
    use tokio::sync::oneshot;

    use crate::core::{
        api::QApi,
        buffer::{debug_buf_to_string, QBufferVec, QVecBuffer},
        config::QConfiguration,
        conn::QConnection,
        listener::QListener,
        reg::QRegistration,
        stream::QStream,
    };

    use tracing::info;

    pub fn get_test_cert_hash() -> String {
        let output = Command::new("pwsh.exe")
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
        s
    }

    // used for debugging
    pub const DEVEL_TRACE_LEVEL: tracing::Level = tracing::Level::TRACE;

    pub fn try_setup_tracing() {
        let _ = tracing_subscriber::fmt()
            .with_max_level(crate::tests::DEVEL_TRACE_LEVEL)
            .try_init();
    }

    #[test]
    fn basic_test() {
        try_setup_tracing();
        info!("Test start");
        let cert_hash = get_test_cert_hash();
        info!("Using cert_hash: [{cert_hash}]");

        let api = QApi::default();

        let app_name = std::ffi::CString::new("testapp").unwrap();

        let config = RegistrationConfig {
            app_name: app_name.as_ptr(),
            execution_profile: EXECUTION_PROFILE_LOW_LATENCY,
        };
        let q_reg = QRegistration::new(&api, &config);

        let args: [QVecBuffer; 1] = ["sample".into()];
        let alpn = QBufferVec::from(args.as_slice());
        let mut settings = Settings::new();
        settings.set_idle_timeout_ms(1000);
        settings.set_peer_bidi_stream_count(1);
        settings.set_server_resumption_level(2); // QUIC_SERVER_RESUME_AND_ZERORTT

        let q_config = QConfiguration::new(&q_reg, alpn.as_buffers(), &settings);
        {
            let mut hash_array: [u8; 20] = [0; 20];
            hex::decode_to_slice(cert_hash.as_bytes(), &mut hash_array).expect("Decoding failed");

            let mut cred_config = CredentialConfig::new_client();
            cred_config.cred_type = CREDENTIAL_TYPE_CERTIFICATE_HASH;
            cred_config.cred_flags = CREDENTIAL_TYPE_NONE;
            cred_config.certificate = CertificateUnion {
                hash: &CertificateHash {
                    sha_hash: hash_array,
                },
            };

            q_config.load_cred(&cred_config);
        }

        let q_req_copy = q_reg.clone();
        let (sht_tx, mut sht_rx) = oneshot::channel::<()>();
        let th = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_time()
                .build()
                .unwrap();
            rt.block_on(async {
                // config needs to be dropped before reg.
                let q_config = q_config;
                let mut l;
                {
                    let local_address = Addr::ipv4(ADDRESS_FAMILY_UNSPEC, 4567_u16.to_be(), 0);
                    l = QListener::open(&q_req_copy, &q_config);
                    info!("Start listener.");
                    let alpn = QBufferVec::from(args.as_slice());
                    l.start(alpn.as_buffers(), &local_address);
                }
                let mut i = 0;
                loop {
                    let conn_id = i;
                    info!("server accept conn {}", i);
                    i += 1;
                    let conn = tokio::select! {
                        val = l.accept() => val,
                        _ = &mut sht_rx => {
                            info!("server accepted interrupted.");
                            None // stop accept and break.
                        }
                    };
                    if conn.is_none() {
                        info!("server accepted conn end");
                        break;
                    }
                    let rth = rt.handle().clone();
                    // use another task to handle conn
                    rt.spawn(async move {
                        let mut conn = conn.unwrap();
                        info!("server accepted conn id={}", conn_id);
                        info!("server conn connect");
                        // TODO: enable this?
                        // conn.proceed().await.unwrap();
                        //tokio::time::sleep(Duration::from_millis(1)).await;
                        //conn.send_resumption_ticket(SEND_RESUMPTION_FLAG_NONE);
                        //tokio::time::sleep(Duration::from_millis(1)).await;
                        loop {
                            info!("server conn accept");
                            let s = conn.accept().await;
                            if s.is_none() {
                                info!("server accept stream end");
                                break;
                            }
                            rth.spawn(async move {
                                info!("server accepted stream");
                                let mut s = s.unwrap();
                                info!("server stream {} receive", s.get_id());
                                let read = s.receive().await.unwrap();
                                let payload = debug_buf_to_string(read);
                                info!("server received len {}", payload.len());
                                assert_eq!(payload, "hello");
                                let args = Bytes::from("hello world");
                                info!("server stream send");
                                s.send(args, SEND_FLAG_FIN).await.unwrap();
                                info!("server stream drain");
                                s.drain().await;
                                info!("server stream end");
                            });
                        }
                        info!("server conn shutdown");
                        conn.shutdown(0).await;
                        info!("server conn shutdown end");
                    });
                }
                info!("server listener stop");
                l.stop().await;
                info!("server listner stop finish");
            });
            info!("tokio server end.");
        });

        thread::sleep(Duration::from_secs(1));

        // open client
        let mut client_settings = Settings::new();
        client_settings.set_idle_timeout_ms(1000);
        let client_config = QConfiguration::new(&q_reg, alpn.as_buffers(), &client_settings);
        {
            let mut cred_config = CredentialConfig::new_client();
            cred_config.cred_type = CREDENTIAL_TYPE_NONE;
            cred_config.cred_flags = CREDENTIAL_FLAG_CLIENT;
            cred_config.cred_flags |= CREDENTIAL_FLAG_NO_CERTIFICATE_VALIDATION;
            client_config.load_cred(&cred_config);
        }
        // run client in another runtime.
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
            .block_on(async move {
                let client_config = client_config;
                tokio::time::sleep(Duration::from_secs(1)).await;
                info!("client conn open");
                let mut conn = QConnection::open(&q_reg);
                info!("client conn start");
                conn.start(&client_config, "localhost", 4567).await.unwrap();

                info!("client stream open");
                let mut st = QStream::open(&conn, STREAM_OPEN_FLAG_NONE);
                info!("client stream start");
                st.start(STREAM_START_FLAG_NONE).await.unwrap();
                let args = Bytes::from("hello");
                info!("client stream {} send", st.get_id());
                st.send(args, SEND_FLAG_FIN).await.unwrap();

                info!("client stream receive");
                let read = st.receive().await.unwrap();
                let payload = debug_buf_to_string(read);
                info!("client stream receive read :{}", payload.len());
                assert_eq!(payload, "hello world");
                info!("client stream drain");
                st.drain().await;
                info!("client conn shutdown");
                conn.shutdown(0).await;
                // shutdown server
                sht_tx.send(()).unwrap();
            });
        th.join().unwrap();
    }
}

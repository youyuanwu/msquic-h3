use std::sync::Arc;

use msquic_sys2::{Buffer, Configuration, CredentialConfig, Settings};

use crate::core::api::QApi;
use crate::core::{reg::QRegistration, utils::SBox};

#[derive(Clone)]
pub struct QConfiguration {
    pub _api: QApi,
    pub inner: Arc<SBox<Configuration>>,
}

impl QConfiguration {
    pub fn new(reg: &QRegistration, alpn: &[Buffer], settings: &Settings) -> Self {
        let c = Configuration::new(&reg.inner.inner, alpn, settings);
        Self {
            _api: reg.api.clone(),
            inner: Arc::new(SBox::new(c)),
        }
    }
    pub fn load_cred(&self, cred_config: &CredentialConfig) {
        self.inner.inner.load_credential(cred_config);
    }
}

//! The ChatGPT sign-ins of the configured endpoints (#620): one token
//! manager per endpoint, made when first asked for and shared by every model
//! on that endpoint. Each renewal replaces the refresh token, so two
//! managers over one file would race; one manager keeps a single renewal at
//! a time.

use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};

use henk_llm::chatgpt::TokenManager;

/// The token managers of the endpoints that sign in with ChatGPT. Cloning
/// shares them.
#[derive(Clone, Default)]
pub struct SignIns {
    managers: Arc<Mutex<BTreeMap<String, Arc<TokenManager>>>>,
}

impl fmt::Debug for SignIns {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SignIns")
    }
}

impl SignIns {
    /// The manager of `endpoint`, whose sign-in is kept in `file`.
    ///
    /// # Errors
    ///
    /// Returns why no HTTP client could be built for it.
    pub fn manager(&self, endpoint: &str, file: &Path) -> Result<Arc<TokenManager>, String> {
        let mut managers = self.managers.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(manager) = managers.get(endpoint) {
            return Ok(Arc::clone(manager));
        }
        let http = henk_llm::build_endpoint_client()
            .map_err(|error| format!("cannot build an HTTP client: {error}"))?;
        let manager = Arc::new(TokenManager::new(endpoint, file, http));
        managers.insert(endpoint.to_owned(), Arc::clone(&manager));
        Ok(manager)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_ask_for_an_endpoint_gets_the_same_manager() {
        let sign_ins = SignIns::default();
        let file = Path::new("/tmp/henk-sign-in.json");
        let first = sign_ins.manager("chatgpt", file).unwrap();
        let again = sign_ins.manager("chatgpt", file).unwrap();
        let clone = sign_ins.clone().manager("chatgpt", file).unwrap();
        let other = sign_ins.manager("second", file).unwrap();
        assert!(Arc::ptr_eq(&first, &again));
        assert!(
            Arc::ptr_eq(&first, &clone),
            "a clone of the settings shares them"
        );
        assert!(!Arc::ptr_eq(&first, &other));
    }
}

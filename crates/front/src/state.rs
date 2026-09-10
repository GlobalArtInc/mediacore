//! What every handler is given: the configuration, the sessions this process
//! is holding, and -- when signing in is switched on -- the provider to sign
//! people in against.
//!
//! The provider is `Option` for the same reason signing in is a switch at all:
//! with it off there is no provider, no session store worth the name and no
//! route that could ask for one, and the page behaves exactly as it did before
//! any of this existed.

use std::sync::Arc;

use crate::config::Config;
use crate::oidc::Provider;
use crate::session::Sessions;

pub struct Front {
    pub config: Config,
    pub sessions: Sessions,
    pub provider: Option<Provider>,
    /// The page with this deployment's brand already in it.
    ///
    /// Rendered once here rather than per request, and rendered at all rather
    /// than painted by a script, because `/api/config` lands after the first
    /// paint: a brand painted from it would show our wordmark for one round
    /// trip on the one screen whose job is to say whose product this is.
    pub page: String,
}

pub type Shared = Arc<Front>;

impl Front {
    pub fn new(config: Config) -> Self {
        let provider = config.oidc.as_ref().map(|oidc| {
            Provider::new(
                &oidc.issuer,
                &oidc.client_id,
                &oidc.client_secret,
                &oidc.redirect_uri(),
                &oidc.scopes,
            )
        });
        let ttl = config
            .oidc
            .as_ref()
            .map_or(crate::session::DEFAULT_SESSION_TTL, |oidc| oidc.session_ttl);

        let page = config.brand.render(crate::http::INDEX_HTML).into_owned();

        Self {
            config,
            sessions: Sessions::new(ttl),
            provider,
            page,
        }
    }

    /// Whether a person has to be signed in to be here. It is the one question
    /// the rest of the crate asks, so that "the switch is on" is one condition
    /// rather than a `config.oidc.is_some()` in every handler that could drift
    /// from a `provider.is_some()` next to it.
    pub fn requires_sign_in(&self) -> bool {
        self.provider.is_some()
    }
}

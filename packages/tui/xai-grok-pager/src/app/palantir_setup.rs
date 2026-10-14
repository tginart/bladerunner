//! First-run Palantir AIP setup on the welcome screen.
//!
//! `/login palantir-aip <hostname> <client-id>` carries its settings as
//! arguments, but the welcome menu's Palantir entry can't. When a sign-in is
//! missing the Foundry hostname, or the OAuth client ID, the welcome screen
//! asks for it in the auth input box before the sign-in starts.

use xai_grok_shell::auth::palantir_aip::{LoginRequest, PalantirAipConfig, normalize_hostname};

/// The setting the welcome screen is asking for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PalantirSetupStep {
    Hostname,
    ClientId,
}

/// A Palantir sign-in waiting on one more setting.
#[derive(Clone, Debug)]
pub(crate) struct PalantirSetup {
    pub request: LoginRequest,
    pub step: PalantirSetupStep,
    pub error: Option<String>,
}

impl PalantirSetup {
    pub(crate) fn title(&self) -> &'static str {
        match self.step {
            PalantirSetupStep::Hostname => "Palantir AIP: enter your Foundry hostname",
            PalantirSetupStep::ClientId => "Palantir AIP: enter the Bladerunner app's client ID",
        }
    }

    pub(crate) fn label(&self) -> &'static str {
        match self.step {
            PalantirSetupStep::Hostname => {
                "The address you open Foundry at, e.g. acme.palantirfoundry.com"
            }
            PalantirSetupStep::ClientId => {
                "From the Bladerunner OAuth app in Developer Console (docs/foundry-app.md). \
                 Leave empty and press enter to use a personal token instead."
            }
        }
    }

    pub(crate) fn placeholder(&self) -> &'static str {
        match self.step {
            PalantirSetupStep::Hostname => "acme.palantirfoundry.com",
            PalantirSetupStep::ClientId => "client ID, or empty for a personal token",
        }
    }

    /// Apply the submitted text. `Err` keeps the same step with a message.
    pub(crate) fn submit(&mut self, text: &str) -> Result<(), String> {
        let text = text.trim();
        match self.step {
            PalantirSetupStep::Hostname => {
                let hostname = normalize_hostname(text).map_err(|e| e.to_string())?;
                self.request.hostname = Some(hostname);
            }
            PalantirSetupStep::ClientId if text.is_empty() => self.request.token = true,
            PalantirSetupStep::ClientId => self.request.client_id = Some(text.to_owned()),
        }
        Ok(())
    }
}

/// What `request` still needs before it can start, given the saved config.
/// A new hostname drops the saved client ID (it belongs to the old Foundry).
pub(crate) fn missing_step(
    request: &LoginRequest,
    config: &PalantirAipConfig,
) -> Option<PalantirSetupStep> {
    let configured = config.require_hostname().ok();
    let hostname = match request.hostname.as_deref() {
        Some(hostname) => normalize_hostname(hostname).ok(),
        None => configured.clone(),
    };
    let Some(hostname) = hostname else {
        return Some(PalantirSetupStep::Hostname);
    };
    if request.token || request.client_id.is_some() {
        return None;
    }
    let keeps_client_id =
        configured.as_deref() == Some(hostname.as_str()) && config.require_client_id().is_ok();
    (!keeps_client_id).then_some(PalantirSetupStep::ClientId)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(hostname: Option<&str>, client_id: Option<&str>) -> PalantirAipConfig {
        PalantirAipConfig {
            hostname: hostname.map(str::to_owned),
            client_id: client_id.map(str::to_owned),
        }
    }

    #[test]
    fn asks_for_the_hostname_first() {
        assert_eq!(
            missing_step(&LoginRequest::default(), &config(None, None)),
            Some(PalantirSetupStep::Hostname)
        );
        let token = LoginRequest {
            token: true,
            ..Default::default()
        };
        assert_eq!(
            missing_step(&token, &config(None, None)),
            Some(PalantirSetupStep::Hostname),
            "a personal token still needs a hostname"
        );
    }

    #[test]
    fn oauth_needs_a_client_id_for_the_hostname_in_use() {
        let saved = config(Some("acme.palantirfoundry.com"), Some("c1"));
        assert_eq!(missing_step(&LoginRequest::default(), &saved), None);

        let new_host = LoginRequest {
            hostname: Some("other.palantirfoundry.com".into()),
            ..Default::default()
        };
        assert_eq!(
            missing_step(&new_host, &saved),
            Some(PalantirSetupStep::ClientId),
            "the saved client ID belongs to the old Foundry"
        );

        let no_client = config(Some("acme.palantirfoundry.com"), None);
        assert_eq!(
            missing_step(&LoginRequest::default(), &no_client),
            Some(PalantirSetupStep::ClientId)
        );
        let token = LoginRequest {
            token: true,
            ..Default::default()
        };
        assert_eq!(missing_step(&token, &no_client), None);
    }

    #[test]
    fn submit_fills_the_request_or_explains() {
        let mut setup = PalantirSetup {
            request: LoginRequest::default(),
            step: PalantirSetupStep::Hostname,
            error: None,
        };
        assert!(setup.submit("not a host/path").is_err());
        setup.submit(" https://Acme.PalantirFoundry.com/ ").unwrap();
        assert_eq!(
            setup.request.hostname.as_deref(),
            Some("acme.palantirfoundry.com")
        );

        setup.step = PalantirSetupStep::ClientId;
        setup.submit("").unwrap();
        assert!(
            setup.request.token,
            "an empty client ID switches to a personal token"
        );
    }
}

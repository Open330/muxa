//! Whether the program on the other end of the daemon socket is the Tend
//! Mac app, as macOS code signing establishes it.
//!
//! Every client of the socket runs as the same user, so a request can name
//! any origin it likes, and an agent's pane is only observed, not proven.
//! For a decision only the person may take from the app (deciding a result
//! that lives on another Fleet host), tendd asks the kernel instead: the
//! peer's audit token, recorded when it connected, must belong to running
//! code that satisfies the app's designated requirement (Developer ID,
//! team, bundle identifier). An agent cannot meet it by naming itself
//! `Tend`, copying the bundle, or connecting first and then `exec`ing the
//! app: the token names the image that connected, and a modified bundle no
//! longer validates.
//!
//! What it does not cover: the real app driven through its own interface
//! (Accessibility automation needs the person's permission grant), and a
//! build signed by anyone else (a local development build is refused).

/// The released app's designated requirement, as
/// `codesign -d -r- /Applications/Tend.app` prints it.
pub const TEND_APP_REQUIREMENT: &str = concat!(
    "identifier \"dev.tend.mac\" and anchor apple generic",
    " and certificate 1[field.1.2.840.113635.100.6.2.6] /* exists */",
    " and certificate leaf[field.1.2.840.113635.100.6.1.13] /* exists */",
    " and certificate leaf[subject.OU] = \"728FW73BS8\"",
);

/// Whether the process that connected with `audit_token` is running code
/// that satisfies [`TEND_APP_REQUIREMENT`]. Blocking (Security framework);
/// call it off the async runtime.
#[must_use]
pub fn is_tend_app(audit_token: Option<[u32; 8]>) -> bool {
    audit_token.is_some_and(|token| satisfies(&token, TEND_APP_REQUIREMENT))
}

#[cfg(target_os = "macos")]
fn satisfies(token: &[u32; 8], requirement: &str) -> bool {
    use core_foundation::base::TCFType;
    use core_foundation::data::CFData;
    use security_framework::os::macos::code_signing::{
        Flags, GuestAttributes, SecCode, SecRequirement,
    };
    let bytes: Vec<u8> = token.iter().flat_map(|word| word.to_ne_bytes()).collect();
    let data = CFData::from_buffer(&bytes);
    let mut attributes = GuestAttributes::new();
    attributes.set_audit_token(data.as_concrete_TypeRef());
    let Ok(code) = SecCode::copy_guest_with_attribues(None, &attributes, Flags::NONE) else {
        return false;
    };
    let Ok(requirement) = requirement.parse::<SecRequirement>() else {
        return false;
    };
    code.check_validity(Flags::NONE, &requirement).is_ok()
}

#[cfg(not(target_os = "macos"))]
fn satisfies(_token: &[u32; 8], _requirement: &str) -> bool {
    false
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn only_code_that_meets_the_requirement_passes() {
        // A real connection: this test binary is the peer, and it is not
        // the signed app.
        let (a, b) = tokio::net::UnixStream::pair().unwrap();
        let token = nix::sys::socket::getsockopt(&b, nix::sys::socket::sockopt::LocalPeerToken)
            .unwrap()
            .val;
        drop(a);
        assert!(!is_tend_app(Some(token)));
        assert!(!is_tend_app(None));
        // The same token does meet a requirement this binary satisfies, so
        // the refusal above is the requirement, not a broken lookup.
        assert!(satisfies(&token, "always"));
        assert!(!satisfies(&token, "never"));
        // A token that names no process is refused.
        assert!(!satisfies(&[0; 8], "always"));
        // The constant parses.
        assert!(TEND_APP_REQUIREMENT
            .parse::<security_framework::os::macos::code_signing::SecRequirement>()
            .is_ok());
    }

    #[test]
    fn the_released_app_meets_the_requirement() {
        use core_foundation::url::CFURL;
        use security_framework::os::macos::code_signing::{Flags, SecRequirement, SecStaticCode};
        let path = std::path::Path::new("/Applications/Tend.app");
        if !path.exists() {
            return; // only where the released app is installed
        }
        let url = CFURL::from_path(path, true).unwrap();
        let code = SecStaticCode::from_path(&url, Flags::NONE).unwrap();
        let requirement: SecRequirement = TEND_APP_REQUIREMENT.parse().unwrap();
        code.check_validity(Flags::NONE, &requirement)
            .expect("the installed Tend.app satisfies the requirement");
    }
}

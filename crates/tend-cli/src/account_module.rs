//! The CLI's door to the optional account module (`tend-aas`, the `aas`
//! cargo feature). Every caller goes through here, so a build or a config
//! without the module answers the same way: the launch starts bare and the
//! swap command says why.

use tend::swap_api::{AccountRoute, RoutedLaunch};

/// Whether this build carries the account module.
pub(crate) const COMPILED: bool = cfg!(feature = "aas");

/// Where a new agent starts. `Err` is why it starts bare.
pub(crate) fn decide(route: &AccountRoute) -> Result<RoutedLaunch, String> {
    #[cfg(feature = "aas")]
    {
        tend_aas::account_route::decide(route)
    }
    #[cfg(not(feature = "aas"))]
    {
        let _ = route;
        Err(tend::swap_api::module_unavailable(false))
    }
}

/// Why `tend swap` and friends refuse, when they do: the module is not in
/// this build, or `config` does not turn it on.
pub(crate) fn unavailable(config: &tend::Config) -> Option<String> {
    if !COMPILED {
        return Some(tend::swap_api::module_unavailable(false));
    }
    (!config.accounts_module().enabled).then(|| tend::swap_api::module_unavailable(true))
}

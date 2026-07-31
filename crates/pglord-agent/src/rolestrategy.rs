//! Role resolution. `auto` derives the role from the StatefulSet pod ordinal:
//! ordinal 0 bootstraps as primary, everything else as standby. This is the
//! MVP topology rule; failover orchestration will override roles at runtime.

use anyhow::bail;

use crate::config::Role;

pub fn resolve_role(role: Role, hostname: &str) -> anyhow::Result<Role> {
    match role {
        Role::Primary | Role::Standby => Ok(role),
        Role::Auto => match hostname.rsplit_once('-') {
            Some((prefix, ordinal)) if !prefix.is_empty() => match ordinal.parse::<u32>() {
                Ok(0) => Ok(Role::Primary),
                Ok(_) => Ok(Role::Standby),
                Err(_) => bail!("role=auto requires an ordinal hostname, got '{hostname}'"),
            },
            _ => bail!("role=auto requires an ordinal hostname, got '{hostname}'"),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinal_zero_is_primary() {
        assert_eq!(
            resolve_role(Role::Auto, "demo-0").expect("resolve"),
            Role::Primary
        );
    }

    #[test]
    fn nonzero_ordinals_are_standby() {
        assert_eq!(
            resolve_role(Role::Auto, "demo-3").expect("resolve"),
            Role::Standby
        );
        assert_eq!(
            resolve_role(Role::Auto, "a-b-2").expect("resolve"),
            Role::Standby
        );
    }

    #[test]
    fn explicit_roles_pass_through() {
        assert_eq!(
            resolve_role(Role::Primary, "whatever").expect("resolve"),
            Role::Primary
        );
        assert_eq!(
            resolve_role(Role::Standby, "demo-0").expect("resolve"),
            Role::Standby
        );
    }

    #[test]
    fn non_ordinal_hostnames_rejected() {
        assert!(resolve_role(Role::Auto, "foo").is_err());
        assert!(resolve_role(Role::Auto, "foo-").is_err());
        assert!(resolve_role(Role::Auto, "-0").is_err());
    }
}

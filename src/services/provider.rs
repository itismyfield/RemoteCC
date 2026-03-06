use crate::utils::format::safe_prefix;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProviderKind {
    Claude,
    Codex,
}

impl ProviderKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }

    pub fn display_name(self) -> &'static str {
        match self {
            Self::Claude => "Claude",
            Self::Codex => "Codex",
        }
    }

    pub fn counterpart(self) -> Self {
        match self {
            Self::Claude => Self::Codex,
            Self::Codex => Self::Claude,
        }
    }

    pub fn from_str(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "claude" => Some(Self::Claude),
            "codex" => Some(Self::Codex),
            _ => None,
        }
    }

    pub fn is_channel_supported(self, channel_name: Option<&str>, is_dm: bool) -> bool {
        if is_dm {
            return true;
        }

        let Some(channel_name) = channel_name else {
            return matches!(self, Self::Claude);
        };

        if channel_name.ends_with("-cdx") {
            return matches!(self, Self::Codex);
        }

        if channel_name.ends_with("-cc") {
            return matches!(self, Self::Claude);
        }

        matches!(self, Self::Claude)
    }

    pub fn build_tmux_session_name(self, channel_name: &str) -> String {
        let sanitized: String = channel_name
            .chars()
            .map(|c| {
                if c.is_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '-'
                }
            })
            .collect();
        let trimmed = safe_prefix(&sanitized, 44);
        format!("remoteCC-{}-{}", self.as_str(), trimmed)
    }
}

pub fn parse_provider_and_channel_from_tmux_name(
    session_name: &str,
) -> Option<(ProviderKind, String)> {
    let stripped = session_name.strip_prefix("remoteCC-")?;
    if let Some(rest) = stripped.strip_prefix("claude-") {
        return Some((ProviderKind::Claude, rest.to_string()));
    }
    if let Some(rest) = stripped.strip_prefix("codex-") {
        return Some((ProviderKind::Codex, rest.to_string()));
    }
    Some((ProviderKind::Claude, stripped.to_string()))
}

#[cfg(test)]
mod tests {
    use super::{parse_provider_and_channel_from_tmux_name, ProviderKind};

    #[test]
    fn test_provider_channel_support() {
        assert!(ProviderKind::Claude.is_channel_supported(Some("mac-mini"), false));
        assert!(ProviderKind::Claude.is_channel_supported(Some("cookingheart-dev-cc"), false));
        assert!(!ProviderKind::Claude.is_channel_supported(Some("cookingheart-dev-cdx"), false));
        assert!(ProviderKind::Codex.is_channel_supported(Some("cookingheart-dev-cdx"), false));
        assert!(!ProviderKind::Codex.is_channel_supported(Some("cookingheart-dev-cc"), false));
        assert!(ProviderKind::Codex.is_channel_supported(None, true));
    }

    #[test]
    fn test_tmux_name_parse_supports_legacy_and_provider_aware_names() {
        assert_eq!(
            parse_provider_and_channel_from_tmux_name("remoteCC-claude-cookingheart-dev-cc"),
            Some((ProviderKind::Claude, "cookingheart-dev-cc".to_string()))
        );
        assert_eq!(
            parse_provider_and_channel_from_tmux_name("remoteCC-codex-cookingheart-dev-cdx"),
            Some((ProviderKind::Codex, "cookingheart-dev-cdx".to_string()))
        );
        assert_eq!(
            parse_provider_and_channel_from_tmux_name("remoteCC-mac-mini"),
            Some((ProviderKind::Claude, "mac-mini".to_string()))
        );
    }
}

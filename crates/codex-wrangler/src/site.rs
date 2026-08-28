use std::fmt::{self, Display, Formatter};

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum Site {
    #[default]
    Local,
    Remote(RemoteSite),
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SessionKey {
    pub site: Site,
    pub thread: String,
}

impl SessionKey {
    pub fn new(site: Site, thread: String) -> Self {
        Self { site, thread }
    }
}

impl Site {
    pub const fn local(&self) -> bool {
        matches!(self, Self::Local)
    }

    pub const fn remote(&self) -> Option<&RemoteSite> {
        match self {
            Self::Local => None,
            Self::Remote(site) => Some(site),
        }
    }
}

impl Display for Site {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Local => formatter.write_str("LOCAL"),
            Self::Remote(site) => site.fmt(formatter),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct RemoteSite(String);

impl RemoteSite {
    pub fn parse(endpoint: &str) -> Result<Self, String> {
        let endpoint = endpoint.trim();
        if endpoint.is_empty() {
            return Err("remote SSH endpoints cannot be empty".to_owned());
        }
        if endpoint.starts_with('-') {
            return Err(format!(
                "remote SSH endpoint `{endpoint}` cannot begin with `-`"
            ));
        }
        if endpoint.chars().any(char::is_whitespace) {
            return Err(format!(
                "remote SSH endpoint `{endpoint}` must be one OpenSSH destination or Host alias"
            ));
        }
        Ok(Self(endpoint.to_owned()))
    }

    pub fn endpoint(&self) -> &str {
        &self.0
    }

    pub fn palette(&self) -> u64 {
        self.0
            .bytes()
            .fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
                (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3)
            })
    }
}

impl Display for RemoteSite {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

use std::{
    ffi::{OsStr, OsString},
    path::Path,
};

use crate::site::{RemoteSite, SessionKey, Site};

const ENV: &str = "/usr/bin/env";
const TRUECOLOR: &str = "COLORTERM=truecolor";
const WRANGLER: &str = "/usr/bin/codex-wrangler";

#[derive(Clone, Copy)]
pub(crate) enum RelayOperation {
    Resume,
    Fork,
}

impl RelayOperation {
    const fn verb(self) -> &'static str {
        match self {
            Self::Resume => "resume",
            Self::Fork => "fork",
        }
    }
}

pub(crate) fn ssh_argv(
    site: &RemoteSite,
    operation: RelayOperation,
    thread: &str,
) -> Vec<OsString> {
    [
        "ssh",
        "-t",
        "--",
        site.endpoint(),
        ENV,
        TRUECOLOR,
        WRANGLER,
        "relay",
        operation.verb(),
        thread,
    ]
    .map(OsString::from)
    .into()
}

pub(crate) fn resumed_session(argv: &[OsString]) -> Option<SessionKey> {
    if Path::new(argv.first()?).file_name()? != OsStr::new("ssh") {
        return None;
    }
    let separator = argv.iter().rposition(|arg| arg == OsStr::new("--"))?;
    let [endpoint, command @ ..] = argv.get(separator + 1..)? else {
        return None;
    };
    // Terminal processes outlive Wrangler upgrades. The shorter arm admits
    // relays opened before Wrangler began declaring truecolor explicitly.
    let command = match command {
        [env, truecolor, command @ ..]
            if Path::new(env).file_name()? == OsStr::new("env")
                && truecolor == OsStr::new(TRUECOLOR) =>
        {
            command
        }
        command => command,
    };
    let [wrangler, relay, resume, thread] = command else {
        return None;
    };
    if Path::new(wrangler).file_name()? != OsStr::new("codex-wrangler")
        || relay != OsStr::new("relay")
        || resume != OsStr::new("resume")
    {
        return None;
    }
    let thread = thread.to_str().filter(|thread| uuid_literal(thread))?;
    let site = RemoteSite::parse(endpoint.to_str()?).ok()?;
    Some(SessionKey::new(Site::Remote(site), thread.to_owned()))
}

fn uuid_literal(text: &str) -> bool {
    text.len() == 36
        && text.bytes().enumerate().all(|(index, byte)| {
            if [8, 13, 18, 23].contains(&index) {
                byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    const THREAD: &str = "019fc940-b18f-7ad2-a012-71d86289bd60";

    #[test]
    fn seat_recognition_and_terminal_launch_share_one_grammar() {
        let site = RemoteSite::parse("main").expect("valid fixture Site");
        let resume = ssh_argv(&site, RelayOperation::Resume, THREAD);
        assert_eq!(
            resumed_session(&resume),
            Some(SessionKey::new(
                Site::Remote(site.clone()),
                THREAD.to_owned()
            ))
        );

        let prior = [
            "ssh", "-t", "--", "main", WRANGLER, "relay", "resume", THREAD,
        ]
        .map(OsString::from);
        assert_eq!(
            resumed_session(&prior),
            Some(SessionKey::new(
                Site::Remote(site.clone()),
                THREAD.to_owned()
            ))
        );

        assert_eq!(
            resumed_session(&ssh_argv(&site, RelayOperation::Fork, THREAD)),
            None
        );
    }
}

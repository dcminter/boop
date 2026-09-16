use std::ffi::OsString;

pub const DEFAULT_SESSION: &str = "boop";
pub const DEFAULT_DETACH_KEY: u8 = 0x1c;

#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    Auto,
    Open {
        session: String,
        command: Vec<OsString>,
    },
    Attach {
        session: String,
    },
    Disconnect {
        session: String,
    },
    List,
    Help,
    Version,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Options {
    pub action: Action,
    pub detach_key: u8,
}

pub const USAGE: &str = "\
Usage: boop [--session NAME] [--detach-key KEY] [COMMAND [ARG...]]
       boop --name NAME [--detach-key KEY]
       boop --disconnect NAME
       boop --list

  -s, --session NAME     attach to or create session NAME
  -n, --name NAME        attach to existing session NAME
  -d, --disconnect NAME  detach all clients of session NAME
  -l, --list             list sessions
  -k, --detach-key KEY   detach key, written ^X (default ^\\)
  -h, --help             show this help
  -V, --version          show the version
";

pub fn parse(args: Vec<OsString>, env_detach_key: Option<&str>) -> Result<Options, String> {
    let mut session = None;
    let mut name = None;
    let mut disconnect = None;
    let mut list = false;
    let mut detach_key = None;
    let mut command = Vec::new();
    let mut args = args.into_iter();

    while let Some(arg) = args.next() {
        let Some(text) = arg.to_str() else {
            command.push(arg);
            break;
        };
        if text == "--" {
            break;
        }
        if !text.starts_with('-') || text == "-" {
            command.push(arg);
            break;
        }
        let (flag, inline) = match text.split_once('=') {
            Some((flag, value)) if flag.starts_with("--") => (flag, Some(value.to_string())),
            _ => (text, None),
        };
        let mut value = |flag: &str| -> Result<String, String> {
            match inline.clone() {
                Some(value) => Ok(value),
                None => args
                    .next()
                    .and_then(|value| value.into_string().ok())
                    .ok_or_else(|| format!("{flag} needs a value")),
            }
        };
        match flag {
            "-s" | "--session" => session = Some(session_name(value(flag)?)?),
            "-n" | "--name" => name = Some(session_name(value(flag)?)?),
            "-d" | "--disconnect" => disconnect = Some(session_name(value(flag)?)?),
            "-k" | "--detach-key" => detach_key = Some(parse_key(&value(flag)?)?),
            "-l" | "--list" => list = true,
            "-h" | "--help" => return Ok(simple(Action::Help)),
            "-V" | "--version" => return Ok(simple(Action::Version)),
            _ => return Err(format!("unknown option {text}")),
        }
        if inline.is_some() && matches!(flag, "--list") {
            return Err(format!("{flag} takes no value"));
        }
    }
    command.extend(args);

    let detach_key = match (detach_key, env_detach_key) {
        (Some(key), _) => key,
        (None, Some(text)) => {
            parse_key(text).map_err(|error| format!("BOOP_DETACH_KEY: {error}"))?
        }
        (None, None) => DEFAULT_DETACH_KEY,
    };

    let chosen = [
        session.is_some(),
        name.is_some(),
        disconnect.is_some(),
        list,
    ];
    if chosen.iter().filter(|&&set| set).count() > 1 {
        return Err("--session, --name, --disconnect and --list are exclusive".into());
    }
    let takes_command = name.is_none() && disconnect.is_none() && !list;
    if !command.is_empty() && !takes_command {
        return Err("a command is only accepted with --session or alone".into());
    }

    let action = if let Some(session) = name {
        Action::Attach { session }
    } else if let Some(session) = disconnect {
        Action::Disconnect { session }
    } else if list {
        Action::List
    } else if session.is_none() && command.is_empty() {
        Action::Auto
    } else {
        Action::Open {
            session: session.unwrap_or_else(|| DEFAULT_SESSION.into()),
            command,
        }
    };
    Ok(Options { action, detach_key })
}

fn simple(action: Action) -> Options {
    Options {
        action,
        detach_key: DEFAULT_DETACH_KEY,
    }
}

pub fn session_name(name: String) -> Result<String, String> {
    let valid = !name.is_empty()
        && name.len() <= 64
        && !name.starts_with('.')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._-+@:".contains(c));
    if valid {
        Ok(name)
    } else {
        Err(format!(
            "invalid session name {name:?}: use up to 64 of A-Z a-z 0-9 . _ - + @ : not starting with ."
        ))
    }
}

pub fn parse_key(text: &str) -> Result<u8, String> {
    let bytes = text.as_bytes();
    match bytes {
        [b'^', b'?'] => Ok(0x7f),
        [b'^', c] if c.to_ascii_uppercase().wrapping_sub(b'@') < 0x20 => {
            Ok(c.to_ascii_uppercase() - b'@')
        }
        _ => Err(format!(
            "invalid key {text:?}: expected ^ and a character from @A-Z[\\]^_?"
        )),
    }
}

pub fn key_name(key: u8) -> String {
    match key {
        0x7f => "^?".into(),
        _ => format!("^{}", char::from(key & 0x1f | 0x40)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(args: &[&str]) -> Result<Options, String> {
        parse(args.iter().map(OsString::from).collect(), None)
    }

    fn action(args: &[&str]) -> Action {
        run(args).unwrap().action
    }

    fn open(session: &str, command: &[&str]) -> Action {
        Action::Open {
            session: session.into(),
            command: command.iter().map(OsString::from).collect(),
        }
    }

    #[test]
    fn bare_invocation_is_auto() {
        assert_eq!(action(&[]), Action::Auto);
        assert_eq!(run(&[]).unwrap().detach_key, DEFAULT_DETACH_KEY);
    }

    #[test]
    fn command_opens_default_session() {
        assert_eq!(action(&["top"]), open("boop", &["top"]));
        assert_eq!(
            action(&["ls", "-l", "--session", "x"]),
            open("boop", &["ls", "-l", "--session", "x"])
        );
    }

    #[test]
    fn double_dash_ends_options() {
        assert_eq!(action(&["--", "--list"]), open("boop", &["--list"]));
        assert_eq!(action(&["--"]), Action::Auto);
    }

    #[test]
    fn session_forms() {
        assert_eq!(action(&["--session", "work"]), open("work", &[]));
        assert_eq!(action(&["--session=work"]), open("work", &[]));
        assert_eq!(
            action(&["-s", "work", "vim", "f"]),
            open("work", &["vim", "f"])
        );
    }

    #[test]
    fn name_attaches() {
        assert_eq!(
            action(&["--name", "work"]),
            Action::Attach {
                session: "work".into()
            }
        );
        assert!(run(&["--name", "work", "top"]).is_err());
    }

    #[test]
    fn disconnect_and_list() {
        assert_eq!(
            action(&["-d", "work"]),
            Action::Disconnect {
                session: "work".into()
            }
        );
        assert_eq!(action(&["--list"]), Action::List);
        assert!(run(&["--list", "top"]).is_err());
        assert!(run(&["--list=x"]).is_err());
        assert!(run(&["--disconnect", "a", "top"]).is_err());
    }

    #[test]
    fn exclusive_options() {
        assert!(run(&["--session", "a", "--name", "b"]).is_err());
        assert!(run(&["--list", "--disconnect", "b"]).is_err());
    }

    #[test]
    fn help_and_version() {
        assert_eq!(action(&["--help"]), Action::Help);
        assert_eq!(action(&["-V"]), Action::Version);
    }

    #[test]
    fn errors() {
        assert!(run(&["--bogus"]).is_err());
        assert!(run(&["--session"]).is_err());
        assert!(run(&["--session", ""]).is_err());
        assert!(run(&["--session", "a/b"]).is_err());
        assert!(run(&["--session", ".."]).is_err());
        assert!(run(&["--session", &"x".repeat(65)]).is_err());
        assert!(run(&["--detach-key", "a"]).is_err());
    }

    #[test]
    fn detach_key_option_and_environment() {
        assert_eq!(run(&["-k", "^a"]).unwrap().detach_key, 1);
        assert_eq!(run(&["--detach-key=^]"]).unwrap().detach_key, 0x1d);
        let env = parse(vec![], Some("^B")).unwrap();
        assert_eq!(env.detach_key, 2);
        let both = parse(vec!["-k".into(), "^C".into()], Some("^B")).unwrap();
        assert_eq!(both.detach_key, 3);
        assert!(parse(vec![], Some("x")).is_err());
    }

    #[test]
    fn key_parsing() {
        assert_eq!(parse_key("^\\"), Ok(0x1c));
        assert_eq!(parse_key("^@"), Ok(0));
        assert_eq!(parse_key("^_"), Ok(0x1f));
        assert_eq!(parse_key("^?"), Ok(0x7f));
        assert_eq!(parse_key("^z"), Ok(0x1a));
        assert!(parse_key("^").is_err());
        assert!(parse_key("^1").is_err());
        assert!(parse_key("^ab").is_err());
        assert!(parse_key("\\").is_err());
    }

    #[test]
    fn key_names() {
        assert_eq!(key_name(0x1c), "^\\");
        assert_eq!(key_name(0), "^@");
        assert_eq!(key_name(1), "^A");
        assert_eq!(key_name(0x1f), "^_");
        assert_eq!(key_name(0x7f), "^?");
        for text in ["^@", "^A", "^Z", "^[", "^\\", "^]", "^^", "^_", "^?"] {
            assert_eq!(key_name(parse_key(text).unwrap()), text);
        }
    }
}

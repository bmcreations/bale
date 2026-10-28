//! Recognising bale's own commands inside a tool's shell command line, by
//! the words sh would see rather than by substring.

/// The words of `cmd` as sh splits them: quotes and backslashes resolved,
/// and each of `; & | ( )` and newline a word of its own. `None` when a
/// quote is left open.
pub(crate) fn words(cmd: &str) -> Option<Vec<String>> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_word = false;
    let flush = |cur: &mut String, in_word: &mut bool, out: &mut Vec<String>| {
        if *in_word {
            out.push(std::mem::take(cur));
            *in_word = false;
        }
    };
    let mut chars = cmd.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                in_word = true;
                loop {
                    match chars.next()? {
                        '\'' => break,
                        c => cur.push(c),
                    }
                }
            }
            '"' => {
                in_word = true;
                loop {
                    match chars.next()? {
                        '"' => break,
                        '\\' => match chars.peek() {
                            Some(&n) if matches!(n, '"' | '\\' | '$' | '`' | '\n') => {
                                cur.push(n);
                                chars.next();
                            }
                            _ => cur.push('\\'),
                        },
                        c => cur.push(c),
                    }
                }
            }
            '\\' => {
                in_word = true;
                match chars.next() {
                    Some('\n') | None => {}
                    Some(n) => cur.push(n),
                }
            }
            ' ' | '\t' => flush(&mut cur, &mut in_word, &mut out),
            ';' | '&' | '|' | '(' | ')' | '\n' => {
                flush(&mut cur, &mut in_word, &mut out);
                out.push(c.to_string());
            }
            _ => {
                in_word = true;
                cur.push(c);
            }
        }
    }
    flush(&mut cur, &mut in_word, &mut out);
    Some(out)
}

/// How a command line runs `bale <args>`.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum BaleCall {
    /// The whole command is bale's exe, then exactly `args`.
    Plain,
    /// bale's exe and `args` appear inside a longer command, e.g. under
    /// `env` or `nice`, or followed by more words.
    Wrapped,
}

/// Whether `word` names bale's binary: its basename is `bale` or `bale.exe`.
pub(crate) fn is_bale(word: &str) -> bool {
    matches!(word.rsplit(['/', '\\']).next(), Some("bale" | "bale.exe"))
}

/// How `cmd` runs bale with `args` (see [`BaleCall`]), or `None` when it
/// does not.
pub(crate) fn bale_call(cmd: &str, args: &[&str]) -> Option<BaleCall> {
    let w = words(cmd)?;
    let runs = |win: &[String]| {
        is_bale(&win[0]) && win[1..].iter().map(String::as_str).eq(args.iter().copied())
    };
    if w.len() == args.len() + 1 && runs(&w) {
        return Some(BaleCall::Plain);
    }
    w.windows(args.len() + 1)
        .any(runs)
        .then_some(BaleCall::Wrapped)
}

/// `quoted` (bale's exe, sh-quoted) running `args`, only when the exe is
/// there: status 0 without it, bale's status with it.
pub(crate) fn sh_guarded(quoted: &str, args: &str) -> String {
    format!("if [ -x {quoted} ]; then {quoted} {args}; fi")
}

/// `quoted` (bale's exe, cmd-quoted) running `args`, only when the exe is
/// there.
pub(crate) fn cmd_guarded(quoted: &str, args: &str) -> String {
    format!("if exist {quoted} {quoted} {args}")
}

/// Whether `word` is an absolute path on any OS bale supports: `/…`,
/// `C:\…` or `C:/…`, or a UNC `\\…` path. Judged by text, not by the host,
/// so a file written on one OS reads the same on another.
pub(crate) fn is_absolute(word: &str) -> bool {
    let b = word.as_bytes();
    word.starts_with('/')
        || word.starts_with(r"\\")
        || (b.len() > 2
            && b[0].is_ascii_alphabetic()
            && b[1] == b':'
            && matches!(b[2], b'\\' | b'/'))
}

/// The exe path when `cmd` is bale's own entry running `args`, from any exe
/// path: the bare call earlier installs wrote, or the form [`sh_guarded`] or
/// [`cmd_guarded`] writes, quoted exactly as bale quotes it. bale only ever
/// writes an absolute exe path, so an entry naming bale by a relative path
/// or bare name, or guarded in a shape bale does not write, is the user's
/// and gives `None`.
pub(crate) fn own_exe(cmd: &str, args: &[&str]) -> Option<String> {
    let w = words(cmd)?;
    let at = match (w.first().map(String::as_str), w.get(1).map(String::as_str)) {
        (Some("if"), Some("[")) => 7,
        (Some("if"), Some("exist")) => 3,
        _ => 0,
    };
    let exe = w.get(at)?;
    if !is_absolute(exe) || !is_bale(exe) {
        return None;
    }
    let joined = args.join(" ");
    let own = if at == 0 {
        w[1..].iter().map(String::as_str).eq(args.iter().copied())
    } else {
        cmd == sh_guarded(&crate::claude::sh_quote(exe), &joined)
            || cmd == cmd_guarded(&crate::install::windows_quote(exe), &joined)
    };
    own.then(|| exe.clone())
}

/// How `cmd` runs bale with `args`: [`BaleCall::Plain`] only for bale's own
/// entry (see [`own_exe`]), [`BaleCall::Wrapped`] for any other command that
/// runs bale with `args`.
pub(crate) fn own_call(cmd: &str, args: &[&str]) -> Option<BaleCall> {
    if own_exe(cmd, args).is_some() {
        return Some(BaleCall::Plain);
    }
    bale_call(cmd, args).map(|_| BaleCall::Wrapped)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words_resolve_quotes_and_split_operators() {
        assert_eq!(
            words(r#"'/a b/it'\''s' "x\"y" c\ d;e&&f"#).unwrap(),
            ["/a b/it's", "x\"y", "c d", ";", "e", "&", "&", "f"]
        );
        assert_eq!(
            words(r#""C:\Program Files\bale.exe" hook"#).unwrap(),
            [r"C:\Program Files\bale.exe", "hook"]
        );
        assert_eq!(
            words("( npm i\n) && x").unwrap()[..4],
            ["(", "npm", "i", "\n"]
        );
        assert_eq!(words("'open"), None);
    }

    #[test]
    fn bale_call_tells_plain_from_wrapped() {
        let args = ["hook", "cursor", "setup"];
        assert_eq!(
            bale_call("'/opt/bale' hook cursor setup", &args),
            Some(BaleCall::Plain)
        );
        assert_eq!(
            bale_call(r#""C:\b\bale.exe" hook cursor setup"#, &args),
            Some(BaleCall::Plain)
        );
        assert_eq!(
            bale_call("env X=1 bale hook cursor setup", &args),
            Some(BaleCall::Wrapped)
        );
        assert_eq!(
            bale_call("bale hook cursor setup; echo", &args),
            Some(BaleCall::Wrapped)
        );
        for not in [
            "echo ' bale hook cursor setup'",
            "'/x/not-bale' hook cursor setup",
            "bale hook cursor setups",
            "bale hook cursor",
            "'open",
        ] {
            assert_eq!(bale_call(not, &args), None, "{not}");
        }
    }

    /// Only an absolute exe, in the exact text bale writes, is bale's own;
    /// a bare name, a relative path or a guard bale did not write is the
    /// user's.
    #[test]
    fn own_exe_needs_an_absolute_path_in_bales_own_form() {
        let args = ["hook", "cursor", "setup"];
        for (cmd, exe) in [
            ("'/opt/bale' hook cursor setup", "/opt/bale"),
            (r#""C:\b\bale.exe" hook cursor setup"#, r"C:\b\bale.exe"),
            (
                "if [ -x '/opt/it'\\''s/bale' ]; then '/opt/it'\\''s/bale' hook cursor setup; fi",
                "/opt/it's/bale",
            ),
            (
                r#"if exist "C:\b\bale.exe" "C:\b\bale.exe" hook cursor setup"#,
                r"C:\b\bale.exe",
            ),
        ] {
            assert_eq!(own_exe(cmd, &args).as_deref(), Some(exe), "{cmd}");
            assert_eq!(own_call(cmd, &args), Some(BaleCall::Plain), "{cmd}");
        }
        for cmd in [
            "bale hook cursor setup",
            "./bin/bale hook cursor setup",
            "'bin/bale.exe' hook cursor setup",
            // Guarded by the user: unquoted, or a relative path.
            "if [ -x /usr/local/bin/bale ]; then /usr/local/bin/bale hook cursor setup; fi",
            "if [ -x 'bale' ]; then 'bale' hook cursor setup; fi",
            "if [ -x '/a/bale' ]; then '/b/bale' hook cursor setup; fi",
            "if exist bale.exe bale.exe hook cursor setup",
        ] {
            assert_eq!(own_exe(cmd, &args), None, "{cmd}");
            assert_eq!(own_call(cmd, &args), Some(BaleCall::Wrapped), "{cmd}");
        }
    }
}

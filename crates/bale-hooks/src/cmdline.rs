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

/// How `cmd` runs bale with `args`, counting the forms [`sh_guarded`] and
/// [`cmd_guarded`] write as [`BaleCall::Plain`] when the guard and the call
/// name the same bale exe.
pub(crate) fn own_call(cmd: &str, args: &[&str]) -> Option<BaleCall> {
    let call = bale_call(cmd, args)?;
    if call == BaleCall::Plain {
        return Some(call);
    }
    let w = words(cmd)?;
    let w: Vec<&str> = w.iter().map(String::as_str).collect();
    let tail = |rest: &[&str]| rest.iter().copied().eq(args.iter().copied());
    let guarded = match w.as_slice() {
        ["if", "[", "-x", guard, "]", ";", "then", exe, rest @ ..] => {
            matches!(rest.split_last(), Some((&"fi", body))
                if matches!(body.split_last(), Some((&";", a)) if tail(a)))
                && guard == exe
                && is_bale(exe)
        }
        ["if", "exist", guard, exe, rest @ ..] => tail(rest) && guard == exe && is_bale(exe),
        _ => false,
    };
    Some(if guarded { BaleCall::Plain } else { call })
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
}

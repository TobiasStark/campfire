//! Kleine Markdown-Teilmenge fuer Nachrichten: **fett**, *kursiv* / _kursiv_, `Code`, Links.
//!
//! Nur `http://` und `https://` werden zu Links; alles andere bleibt Text. Nicht geschlossene
//! Markierungen bleiben woertlich stehen. Verschachtelung gibt es nicht.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Span {
    Text(String),
    Bold(String),
    Italic(String),
    Code(String),
    Link(String),
}

/// Zeichen, die einen Link am Ende nicht mehr zugehoerig beenden.
const TRAILING: &[char] = &['.', ',', ';', ':', '!', '?', ')', ']', '"', '\''];

fn push_text(out: &mut Vec<Span>, buf: &mut String) {
    if !buf.is_empty() {
        out.push(Span::Text(std::mem::take(buf)));
    }
}

fn find_close(chars: &[char], from: usize, marker: &[char]) -> Option<usize> {
    let mut i = from;
    while i + marker.len() <= chars.len() {
        if chars[i..i + marker.len()] == *marker {
            return (i > from).then_some(i);
        }
        i += 1;
    }
    None
}

pub fn parse(src: &str) -> Vec<Span> {
    let chars: Vec<char> = src.chars().collect();
    let mut out = Vec::new();
    let mut buf = String::new();
    let mut i = 0;
    while i < chars.len() {
        let rest = &chars[i..];
        let starts =
            |p: &str| p.chars().zip(rest.iter()).all(|(a, b)| a == *b) && rest.len() >= p.len();
        // Link
        if (starts("http://") || starts("https://"))
            && (i == 0 || chars[i - 1].is_whitespace() || chars[i - 1] == '(')
        {
            let mut end = i;
            while end < chars.len() && !chars[end].is_whitespace() {
                end += 1;
            }
            while end > i && TRAILING.contains(&chars[end - 1]) {
                end -= 1;
            }
            let url: String = chars[i..end].iter().collect();
            let host_len = url.split("://").nth(1).map_or(0, str::len);
            if host_len > 0 {
                push_text(&mut out, &mut buf);
                out.push(Span::Link(url));
                i = end;
                continue;
            }
        }
        // Markierungen: Code, fett, kursiv
        let (marker, mk): (&[char], fn(String) -> Span) = if chars[i] == '`' {
            (&['`'], Span::Code)
        } else if starts("**") {
            (&['*', '*'], Span::Bold)
        } else if chars[i] == '*' || chars[i] == '_' {
            if chars[i] == '*' {
                (&['*'], Span::Italic)
            } else {
                (&['_'], Span::Italic)
            }
        } else {
            buf.push(chars[i]);
            i += 1;
            continue;
        };
        // "_" mitten im Wort (snake_case) ist keine Kursivmarkierung.
        let word_inside = chars[i] == '_' && i > 0 && chars[i - 1].is_alphanumeric();
        match (!word_inside)
            .then(|| find_close(&chars, i + marker.len(), marker))
            .flatten()
        {
            Some(close) => {
                push_text(&mut out, &mut buf);
                out.push(mk(chars[i + marker.len()..close].iter().collect()));
                i = close + marker.len();
            }
            None => {
                buf.push(chars[i]);
                i += 1;
            }
        }
    }
    push_text(&mut out, &mut buf);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use Span::*;

    fn t(s: &str) -> Span {
        Text(s.into())
    }

    #[test]
    fn basic_marks() {
        assert_eq!(
            parse("a **fett** b *schief* c `code` d _auch_"),
            vec![
                t("a "),
                Bold("fett".into()),
                t(" b "),
                Italic("schief".into()),
                t(" c "),
                Code("code".into()),
                t(" d "),
                Italic("auch".into())
            ]
        );
    }

    #[test]
    fn unclosed_and_empty_marks_stay_literal() {
        assert_eq!(parse("2 * 3 = 6"), vec![t("2 * 3 = 6")]);
        assert_eq!(parse("**offen"), vec![t("**offen")]);
        assert_eq!(parse("a ** b"), vec![t("a ** b")]);
        assert_eq!(parse("snake_case_name"), vec![t("snake_case_name")]);
    }

    #[test]
    fn code_keeps_marks_inside() {
        assert_eq!(parse("`a *b* c`"), vec![Code("a *b* c".into())]);
    }

    #[test]
    fn links_only_http_and_without_trailing_punctuation() {
        assert_eq!(
            parse("siehe https://example.com/a?b=1, ok"),
            vec![
                t("siehe "),
                Link("https://example.com/a?b=1".into()),
                t(", ok")
            ]
        );
        assert_eq!(
            parse("(http://x.de/y)"),
            vec![t("("), Link("http://x.de/y".into()), t(")")]
        );
        assert_eq!(parse("javascript:alert(1)"), vec![t("javascript:alert(1)")]);
        assert_eq!(parse("file:///c:/x"), vec![t("file:///c:/x")]);
        assert_eq!(parse("https://"), vec![t("https://")]);
        assert_eq!(parse("abchttps://x.de"), vec![t("abchttps://x.de")]);
    }

    #[test]
    fn unicode_is_safe() {
        assert_eq!(parse("**ä🎉**"), vec![Bold("ä🎉".into())]);
    }
}

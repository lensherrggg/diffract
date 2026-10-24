use unicode_normalization::UnicodeNormalization;

/// 仅用于匹配的归一化：NFKC、大小写、常见中文标点、空白折叠、段首编号剥离。
/// 输出/展示永远使用原文，此函数的结果不进入报告。
pub fn normalize_for_match(input: &str) -> String {
    let nfkc: String = input.nfkc().collect();
    let lowered = nfkc.to_lowercase();
    let mut body = strip_leading_numbering(lowered.trim());
    // 段首编号有时与列表符号叠用，再剥一轮。
    body = strip_leading_numbering(body.trim());

    let mut out = String::with_capacity(body.len());
    let mut pending_space = false;
    for ch in body.chars() {
        if matches!(ch, '\u{feff}' | '\u{200b}' | '\u{200c}' | '\u{200d}') {
            continue;
        }
        if ch.is_whitespace() {
            if !out.is_empty() {
                pending_space = true;
            }
            continue;
        }
        if pending_space {
            let previous_is_punctuation = out.chars().last().is_some_and(is_match_punctuation);
            if !previous_is_punctuation {
                out.push(' ');
            }
            pending_space = false;
        }
        match map_punctuation(ch) {
            Some(mapped) => {
                if is_match_punctuation(mapped) {
                    let len = out.trim_end_matches(' ').len();
                    out.truncate(len);
                }
                out.push(mapped)
            }
            None => {}
        }
    }
    out
}

fn is_match_punctuation(ch: char) -> bool {
    matches!(
        ch,
        ',' | '.' | ';' | ':' | '!' | '?' | '(' | ')' | '[' | ']' | '<' | '>' | '"' | '\''
    )
}

fn map_punctuation(ch: char) -> Option<char> {
    let mapped = match ch {
        '。' | '｡' | '．' => '.',
        '、' | '，' => ',',
        '；' | '﹔' => ';',
        '：' => ':',
        '！' => '!',
        '？' => '?',
        '（' => '(',
        '）' => ')',
        '【' | '〔' | '〖' => '[',
        '】' | '〕' | '〗' => ']',
        '《' | '〈' => '<',
        '》' | '〉' => '>',
        '“' | '”' | '「' | '」' | '『' | '』' => '"',
        '‘' | '’' => '\'',
        '—' | '–' | '−' | '－' => '-',
        '…' => '.',
        '·' | '•' => '.',
        '　' => ' ',
        other => other,
    };
    Some(mapped)
}

fn is_cjk_numeral(ch: char) -> bool {
    matches!(
        ch,
        '零' | '〇'
            | '一'
            | '二'
            | '三'
            | '四'
            | '五'
            | '六'
            | '七'
            | '八'
            | '九'
            | '十'
            | '百'
            | '千'
            | '万'
            | '亿'
            | '两'
    )
}

/// 剥离段首编号，如“第十二条”“第三条”“一、”“（三）”“1.”“1、”。
/// 只处理段首、只处理常见编号形态，避免误伤正文中的数字。
pub fn strip_leading_numbering(input: &str) -> &str {
    let s = input.trim_start();
    let chars: Vec<(usize, char)> = s.char_indices().collect();
    if chars.is_empty() {
        return s;
    }

    // “第X条/款/项/章/节/编/部分”
    if chars[0].1 == '第' {
        let mut i = 1;
        let mut consumed = false;
        while i < chars.len() {
            let ch = chars[i].1;
            if is_cjk_numeral(ch) || ch.is_ascii_digit() || matches!(ch, '.' | '．' | '-' | ' ') {
                consumed = true;
                i += 1;
                continue;
            }
            break;
        }
        if consumed
            && i < chars.len()
            && matches!(chars[i].1, '章' | '节' | '条' | '款' | '项' | '编')
        {
            i += 1;
            if i < chars.len()
                && matches!(
                    chars[i].1,
                    '之' | '、' | '，' | ',' | '.' | '．' | ':' | '：' | ' ' | '　'
                )
            {
                i += 1;
                while i < chars.len() && chars[i].1.is_whitespace() {
                    i += 1;
                }
            }
            let byte = chars.get(i).map(|(b, _)| *b).unwrap_or(s.len());
            return &s[byte..];
        }
    }

    // “（一）”“(1)”
    if matches!(chars[0].1, '(' | '（') {
        let mut i = 1;
        let mut consumed = false;
        while i < chars.len() {
            let ch = chars[i].1;
            if is_cjk_numeral(ch) || ch.is_ascii_digit() || matches!(ch, '.' | '．') {
                consumed = true;
                i += 1;
                continue;
            }
            break;
        }
        if consumed && i < chars.len() && matches!(chars[i].1, ')' | '）') {
            i += 1;
            while i < chars.len() && chars[i].1.is_whitespace() {
                i += 1;
            }
            let byte = chars.get(i).map(|(b, _)| *b).unwrap_or(s.len());
            return &s[byte..];
        }
    }

    // “一、”“1.”“1、”“1)”
    let mut i = 0;
    let mut had_digit = false;
    while i < chars.len() {
        let ch = chars[i].1;
        if is_cjk_numeral(ch) || ch.is_ascii_digit() {
            had_digit = true;
            i += 1;
            continue;
        }
        if had_digit && matches!(ch, '.' | '．' | '、' | ',' | '，' | ')' | '）') {
            i += 1;
            // 支持 “1.1”“1.2.3” 这类多级数字，但只在后接分隔符并继续数字时吸收。
            loop {
                let start = i;
                while i < chars.len() && chars[i].1.is_ascii_digit() {
                    i += 1;
                }
                if i > start
                    && i < chars.len()
                    && matches!(chars[i].1, '.' | '．')
                    && chars.get(i + 1).is_some_and(|(_, c)| c.is_ascii_digit())
                {
                    i += 1;
                    continue;
                }
                break;
            }
            while i < chars.len() && chars[i].1.is_whitespace() {
                i += 1;
            }
            let byte = chars.get(i).map(|(b, _)| *b).unwrap_or(s.len());
            return &s[byte..];
        }
        break;
    }

    // 列表符号
    if matches!(chars[0].1, '-' | '*' | '•' | '·') {
        if chars.get(1).is_some_and(|(_, ch)| ch.is_whitespace()) {
            let mut i = 1;
            while i < chars.len() && chars[i].1.is_whitespace() {
                i += 1;
            }
            let byte = chars.get(i).map(|(b, _)| *b).unwrap_or(s.len());
            return &s[byte..];
        }
    }

    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_article_numbering() {
        assert_eq!(
            strip_leading_numbering("第十二条 为规范公司管理"),
            "为规范公司管理"
        );
        assert_eq!(strip_leading_numbering("第三条：适用范围"), "适用范围");
        assert_eq!(strip_leading_numbering("一、总则"), "总则");
        assert_eq!(strip_leading_numbering("（三）职责分工"), "职责分工");
        assert_eq!(strip_leading_numbering("(1) 目的"), "目的");
        assert_eq!(strip_leading_numbering("1.2 适用范围"), "适用范围");
        assert_eq!(strip_leading_numbering("第十二条"), "");
    }

    #[test]
    fn does_not_strip_content_numbers() {
        assert_eq!(
            strip_leading_numbering("公司2025年营业收入"),
            "公司2025年营业收入"
        );
        assert_eq!(
            strip_leading_numbering("达标率应不低于90%"),
            "达标率应不低于90%"
        );
    }

    #[test]
    fn normalizes_width_and_punctuation() {
        let a = normalize_for_match("第十二条　为规范公司管理，特制定本规定。");
        let b = normalize_for_match("第 12 条 为规范公司管理, 特制定本规定.");
        assert_eq!(a, b);
        assert_eq!(a, "为规范公司管理,特制定本规定.");
    }

    #[test]
    fn collapses_whitespace_between_cjk() {
        assert_eq!(normalize_for_match("甲   乙\t丙"), "甲 乙 丙");
    }

    #[test]
    fn keeps_hash_heading_as_content() {
        assert_eq!(normalize_for_match("# 总则"), "# 总则");
    }
}

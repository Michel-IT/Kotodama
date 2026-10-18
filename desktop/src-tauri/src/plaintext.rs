//! Markdown answer -> plain text for the places that are not Kotodama: what a shortcut pastes into another
//! program (a notepad, a mail, a Word document) has no use for asterisks, backticks or table bars.
//!
//! The chat inside the app keeps the Markdown; only text that leaves the app goes through here. The rule that
//! matters most: a mark is removed only when it has its partner on the same line, and addresses and code are never
//! touched, so a file name (`__init__.py`, `_config.yml`), a link (`/_next/static`), `2**10` or `*.tmp` come out as
//! they went in.

/// Plain text of a Markdown answer: emphasis marks, heading hashes and backticks dropped, links as `text (address)`,
/// bullet lists with a dash, tables as aligned columns, code blocks left exactly as they are.
pub fn markdown_to_plain(md: &str) -> String {
    let lines: Vec<&str> = md.lines().collect();
    // (line, verbatim): a code line is never trimmed, merged or edited
    let mut out: Vec<(String, bool)> = Vec::new();
    let mut i = 0;
    let mut fence: Option<String> = None;
    while i < lines.len() {
        let line = lines[i];
        let t = line.trim_start();
        if let Some(f) = &fence {
            // a fence is closed by a line of the same character, at least as long, and nothing else (no info string)
            let bare = t.trim_end();
            if bare.len() >= f.len() && bare.chars().all(|c| Some(c) == f.chars().next()) {
                fence = None;
            } else {
                out.push((line.to_string(), true));
            }
            i += 1;
            continue;
        }
        if t.starts_with("```") || t.starts_with("~~~") {
            // the whole run of fence characters: a block opened with four backticks is closed only by four or more
            let first = t.chars().next().unwrap_or('`');
            let run: String = t.chars().take_while(|c| *c == first).collect();
            // the info string of a backtick fence holds no backtick: ```` ``` inline ``` ```` is a code span, not a fence
            if !(first == '`' && t[run.len()..].contains('`')) {
                fence = Some(run);
                i += 1;
                continue;
            }
        }
        // a table is a block of lines starting with a bar whose second line is the `---` separator; anything else
        // that starts with a bar (`|x| is the absolute value`) is a normal line
        if t.starts_with('|') {
            let mut rows: Vec<&str> = Vec::new();
            let mut j = i;
            while j < lines.len() && lines[j].trim_start().starts_with('|') {
                rows.push(lines[j]);
                j += 1;
            }
            if rows.len() >= 2 && is_separator(&split_cells(rows[1])) {
                out.extend(table_to_plain(&rows).into_iter().map(|l| (l, false)));
                i = j;
                continue;
            }
        }
        out.push((block_line(line), false));
        i += 1;
    }
    // a run of blank lines outside code is one blank line; leading and trailing blank lines go
    let mut lines_out: Vec<String> = Vec::new();
    let mut blank = false;
    for (l, verbatim) in out {
        if !verbatim && l.trim().is_empty() {
            if !blank {
                lines_out.push(String::new());
            }
            blank = true;
        } else {
            lines_out.push(if verbatim { l } else { l.trim_end().to_string() });
            blank = false;
        }
    }
    while lines_out.first().map_or(false, |l| l.trim().is_empty()) {
        lines_out.remove(0);
    }
    while lines_out.last().map_or(false, |l| l.trim().is_empty()) {
        lines_out.pop();
    }
    lines_out.join("\n")
}

/// One line outside code and tables: heading hashes, rules, quote marks, bullets, then the inline marks.
fn block_line(line: &str) -> String {
    let indent: String = line.chars().take_while(|c| *c == ' ' || *c == '\t').collect();
    let t = line.trim_start();
    let hashes = t.chars().take_while(|c| *c == '#').count();
    if (1..=6).contains(&hashes) && t[hashes..].starts_with(' ') {
        return inline(t[hashes..].trim());
    }
    if t.chars().all(|c| c == '-' || c == '*' || c == '_' || c == ' ') && t.chars().filter(|c| !c.is_whitespace()).count() >= 3 {
        return String::new(); // a horizontal rule
    }
    // a quote keeps its mark: in plain text it is the only way to tell a quotation from the rest
    if t.starts_with('>') {
        let marks: String = t.chars().take_while(|c| *c == '>' || *c == ' ').collect();
        let depth = marks.chars().filter(|c| *c == '>').count();
        return format!("{}{} {}", indent, vec![">"; depth].join(" "), inline(t[marks.len()..].trim_start()));
    }
    for b in ["- ", "* ", "+ "] {
        if let Some(rest) = t.strip_prefix(b) {
            return format!("{indent}- {}", inline(rest));
        }
    }
    format!("{indent}{}", inline(t))
}

/// The inline marks of one line of text. Code spans are kept as they are (backticks dropped); an odd backtick has
/// no partner and stays. Everywhere else a link becomes `text (address)` and paired emphasis marks go.
fn inline(s: &str) -> String {
    // Read from left to right like CommonMark. Outside a code span `\`` is a plain backtick (what the page reading
    // writes for a backtick that is shown as such). A run of N backticks opens a span that ends at the next run of
    // exactly N (`` `echo `date`` `` holds a backtick; inside a span a backslash means nothing: `C:\`); a run with no
    // match is plain text. Each span is parked behind a placeholder while links and emphasis are read, so a mark
    // around code (`**`npm install`**`) still pairs, then the code is put back untouched.
    let c: Vec<char> = s.chars().collect();
    // Every span gets its OWN placeholder (a private-use character the text does not already use), so two spans can
    // never be taken for the same text (`[`a`](`b`)`) and the put-back is a lookup, not a count.
    let mut free = ('\u{E000}'..='\u{F8FF}').filter(|p| !c.contains(p));
    let (mut plain, mut spans) = (String::new(), std::collections::HashMap::<char, String>::new());
    let mut i = 0;
    while i < c.len() {
        if c[i] == '\\' && c.get(i + 1) == Some(&'`') {
            plain.push_str("\\`");
            i += 2;
        } else if c[i] == '`' {
            let run = c[i..].iter().take_while(|x| **x == '`').count();
            let closing = find_closing_run(&c, i + run, run);
            match (closing, free.next()) {
                (Some(close), Some(ph)) => {
                    let mut code: String = c[i + run..close].iter().collect();
                    // one space on each side is the padding that lets a span start or end with a backtick
                    if code.len() > 2 && code.starts_with(' ') && code.ends_with(' ') && code.trim() != "" {
                        code = code[1..code.len() - 1].to_string();
                    }
                    spans.insert(ph, code);
                    plain.push(ph);
                    i = close + run;
                }
                _ => {
                    plain.extend(std::iter::repeat('`').take(run));
                    i += run;
                }
            }
        } else {
            plain.push(c[i]);
            i += 1;
        }
    }
    let read = emphasis(&links(&plain));
    let mut out = String::new();
    for ch in read.chars() {
        match spans.get(&ch) {
            Some(code) => out.push_str(code),
            None => out.push(ch),
        }
    }
    out
}

/// A private-use character: stands for a code span while the text around it is read (counted as a letter).
fn is_placeholder(c: char) -> bool {
    ('\u{E000}'..='\u{F8FF}').contains(&c)
}

/// Index of the next run of exactly `run` backticks at or after `from` (the end of a code span).
fn find_closing_run(c: &[char], from: usize, run: usize) -> Option<usize> {
    let mut j = from;
    while j < c.len() {
        if c[j] == '`' {
            let n = c[j..].iter().take_while(|x| **x == '`').count();
            if n == run {
                return Some(j);
            }
            j += n;
        } else {
            j += 1;
        }
    }
    None
}

/// `[text](address)` ->`text (address)`; the same text and address -> the address alone; an image the same way. The
/// address is read with balanced parentheses (`Rust_(programming_language)`).
fn links(s: &str) -> String {
    let c: Vec<char> = s.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < c.len() {
        let image = c[i] == '!' && c.get(i + 1) == Some(&'[');
        if c[i] == '[' || image {
            let open = if image { i + 1 } else { i };
            if let Some(close) = (open + 1..c.len()).find(|&k| c[k] == ']') {
                if c.get(close + 1) == Some(&'(') {
                    let mut depth = 0;
                    let end = (close + 2..c.len()).find(|&k| match c[k] {
                        '(' => {
                            depth += 1;
                            false
                        }
                        ')' if depth == 0 => true,
                        ')' => {
                            depth -= 1;
                            false
                        }
                        _ => false,
                    });
                    if let Some(end) = end {
                        let text: String = c[open + 1..close].iter().collect();
                        let url: String = c[close + 2..end].iter().collect();
                        let url = url.trim().to_string();
                        if text.trim().is_empty() || text.trim() == url {
                            out.push_str(&url);
                        } else {
                            out.push_str(&format!("{} ({})", text.trim(), url));
                        }
                        i = end + 1;
                        continue;
                    }
                }
            }
        }
        out.push(c[i]);
        i += 1;
    }
    out
}

/// A run of the same mark (`*`, `_` or `~`) and whether it can open or close emphasis.
struct Run {
    start: usize,
    len: usize,
    mark: char,
    open: bool,
    close: bool,
    drop: bool,
}

/// Emphasis marks removed in pairs. A run of marks opens emphasis when text follows it right away and closes it when
/// text comes right before it; it is removed only together with the run of the same kind that matches it. A mark with
/// no partner (`Prezzo* 100`, `*.tmp`), a mark inside a word (`2**10`, `a*b`, `snake_case`), anything inside an
/// address and anything that reads as a file name (`__init__.py`) stays as written. A backslash escape (`\*`) gives
/// the plain character back.
fn emphasis(s: &str) -> String {
    let src: Vec<char> = s.chars().collect();
    // Words that are addresses or paths are not touched at all, escapes included: `https://a.it/_x`, `./*tmp*`,
    // `/var/*cache*`, `dir/*`, `C:\temp\*.tmp`, `\\server\_share`. A word is one when it has "://", a `*`/`_` right
    // next to a slash, a backslash before an ordinary character, or starts with two backslashes.
    let mut path_word = vec![false; src.len()];
    let mut from = 0;
    while from < src.len() {
        if src[from].is_whitespace() {
            from += 1;
            continue;
        }
        let to = (from..src.len()).find(|&k| src[k].is_whitespace()).unwrap_or(src.len());
        let w = &src[from..to];
        let is_path = w.windows(3).any(|t| t == [':', '/', '/'])
            || w.windows(3).any(|t| t[0].is_ascii_alphabetic() && t[1] == ':' && t[2] == '\\') // a drive: `C:\`
            || w.iter().enumerate().any(|(j, &c)| {
                (c == '*' || c == '_') && ((j > 0 && w[j - 1] == '/') || w.get(j + 1) == Some(&'/'))
            })
            || w.starts_with(&['\\', '\\'])
            || w.iter().enumerate().any(|(j, &c)| c == '\\' && w.get(j + 1).map_or(false, |n| !"*_~#`\\".contains(*n)));
        if is_path {
            // emphasis marks around the whole word (`**C:\temp**`) are not part of the path
            let mut lo = from;
            while lo < to && "*~".contains(src[lo]) {
                lo += 1;
            }
            let mut hi = to;
            while hi > lo && "*~".contains(src[hi - 1]) {
                hi -= 1;
            }
            path_word[lo..hi].iter_mut().for_each(|p| *p = true);
        }
        from = to;
    }
    // escapes: (character, was escaped); `protected` follows the characters
    let mut ch: Vec<(char, bool)> = Vec::new();
    let mut protected: Vec<bool> = Vec::new();
    let mut i = 0;
    while i < src.len() {
        // `\**` is a path or a name followed by closing bold (`**C:\Program Files\**`), not an escaped asterisk
        let before_run = i + 2 < src.len() && src[i + 1] == src[i + 2] && "*~".contains(src[i + 1]);
        if !path_word[i] && !before_run && src[i] == '\\' && i + 1 < src.len() && "*_~#`".contains(src[i + 1]) {
            ch.push((src[i + 1], true));
            protected.push(false);
            i += 2;
        } else {
            ch.push((src[i], false));
            protected.push(path_word[i]);
            i += 1;
        }
    }
    let at = |k: isize| -> Option<char> { if k < 0 { None } else { ch.get(k as usize).map(|x| x.0) } };
    let ws = |c: Option<char>| c.map_or(true, |x| x.is_whitespace());
    // a parked code span counts as a letter: `**`x`**` is a word with marks around it
    let word = |c: Option<char>| c.map_or(false, |x| x.is_alphanumeric() || is_placeholder(x));
    let punct = |c: Option<char>| c.map_or(false, |x| !x.is_alphanumeric() && !x.is_whitespace() && !is_placeholder(x));
    let mut runs: Vec<Run> = Vec::new();
    let mut k = 0;
    while k < ch.len() {
        let (c, esc) = ch[k];
        if esc || protected[k] || !(c == '*' || c == '_' || c == '~') {
            k += 1;
            continue;
        }
        let mut len = 1;
        while k + len < ch.len() && ch[k + len] == (c, false) && !protected[k + len] {
            len += 1;
        }
        let (prev, next) = (at(k as isize - 1), at((k + len) as isize));
        let left = !ws(next) && (!punct(next) || ws(prev) || punct(prev));
        let right = !ws(prev) && (!punct(prev) || ws(next) || punct(next));
        let (open, close) = match c {
            '~' if len == 2 => (left, right),
            '~' => (false, false),
            '*' if word(prev) && word(next) => (false, false), // inside a word: `2**10`, `a*b`
            // a `*` that starts a glob (`*.txt`, `*_backup*`) does not open emphasis
            '*' => (left && !matches!(next, Some('.' | '/' | '\\' | '_')), right),
            // `_` is never emphasis here: the assistants write emphasis with `*`, and an underscore is far more often
            // part of a name (`__init__`, `_config.yml`) that must come out as it went in
            _ => (false, false),
        };
        runs.push(Run { start: k, len, mark: c, open, close, drop: false });
        k += len;
    }
    // pair each closer with the nearest opener of the same kind (`*`/`_` of length 1, 2 or 3+; `~~`)
    let kind = |r: &Run| r.len.min(3);
    let mut stack: Vec<usize> = Vec::new();
    for r in 0..runs.len() {
        if runs[r].close {
            if let Some(pos) = stack.iter().rposition(|&o| runs[o].mark == runs[r].mark && kind(&runs[o]) == kind(&runs[r])) {
                let o = stack[pos];
                runs[o].drop = true;
                runs[r].drop = true;
                stack.truncate(pos); // openers above the match are unmatched: they stay as written
                continue;
            }
        }
        if runs[r].open {
            stack.push(r);
        }
    }
    let mut skip = vec![false; ch.len()];
    for r in runs.iter().filter(|r| r.drop) {
        skip[r.start..r.start + r.len].iter_mut().for_each(|x| *x = true);
    }
    ch.iter().enumerate().filter(|(n, _)| !skip[*n]).map(|(_, x)| x.0).collect()
}

/// The cells of one table row: split on the bars that are not escaped (`\|`) and not inside a code span.
fn split_cells(row: &str) -> Vec<String> {
    let t = row.trim();
    let t = t.strip_prefix('|').unwrap_or(t);
    let c: Vec<char> = t.chars().collect();
    let (mut cells, mut cur) = (Vec::new(), String::new());
    let mut i = 0;
    while i < c.len() {
        match c[i] {
            '\\' if c.get(i + 1) == Some(&'|') => {
                cur.push('|');
                i += 2;
            }
            '\\' if c.get(i + 1) == Some(&'`') => {
                cur.push_str("\\`");
                i += 2;
            }
            '`' => {
                // a code span is copied whole (its bars are not cell borders); a run with no match is plain
                let run = c[i..].iter().take_while(|x| **x == '`').count();
                let end = find_closing_run(&c, i + run, run).map_or(i + run, |close| close + run);
                let span: String = c[i..end].iter().collect();
                cur.push_str(&span.replace("\\|", "|"));
                i = end;
            }
            '|' => {
                cells.push(std::mem::take(&mut cur));
                i += 1;
            }
            x => {
                cur.push(x);
                i += 1;
            }
        }
    }
    if !cur.trim().is_empty() {
        cells.push(cur);
    }
    cells
}

fn is_separator(cells: &[String]) -> bool {
    !cells.is_empty() && cells.iter().all(|c| {
        let t = c.trim();
        !t.is_empty() && t.contains('-') && t.chars().all(|x| x == '-' || x == ':')
    })
}

/// Columns a character takes on screen: 2 for East Asian wide characters and emoji, 1 otherwise.
fn cell_width(s: &str) -> usize {
    s.chars()
        .map(|c| match c as u32 {
            0x1100..=0x115F | 0x2E80..=0xA4CF | 0xAC00..=0xD7A3 | 0xF900..=0xFAFF | 0xFE30..=0xFE6F | 0xFF00..=0xFF60 | 0xFFE0..=0xFFE6 | 0x1F300..=0x1FAFF | 0x20000..=0x3FFFD => 2,
            _ => 1,
        })
        .sum()
}

/// A table as aligned columns: the header kept, the separator row dropped, a column whose body cells are all numbers
/// or amounts aligned to the right.
fn table_to_plain(rows: &[&str]) -> Vec<String> {
    let mut body: Vec<Vec<String>> = Vec::new();
    for (n, r) in rows.iter().enumerate() {
        let cells = split_cells(r);
        if n == 1 {
            continue; // the separator row
        }
        body.push(cells.iter().map(|c| inline(c.trim())).collect());
    }
    let cols = body.iter().map(|r| r.len()).max().unwrap_or(0);
    for r in &mut body {
        r.resize(cols, String::new());
    }
    let width: Vec<usize> = (0..cols).map(|k| body.iter().map(|r| cell_width(&r[k])).max().unwrap_or(0)).collect();
    let numeric = |s: &str| s.chars().next().map_or(false, |x| x.is_ascii_digit() || "+-€$£".contains(x));
    let right: Vec<bool> = (0..cols).map(|k| body.len() > 1 && body.iter().skip(1).all(|r| numeric(&r[k]))).collect();
    body.iter()
        .map(|r| {
            let line: Vec<String> = (0..cols)
                .map(|k| {
                    let pad = " ".repeat(width[k] - cell_width(&r[k]));
                    if right[k] { format!("{pad}{}", r[k]) } else { format!("{}{pad}", r[k]) }
                })
                .collect();
            line.join("  ").trim_end().to_string()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_answer_of_the_mockup_reads_as_text() {
        let md = "**Riepilogo del preventivo**\n\nEcco i punti **importanti**:\n\n- Consegna entro il *15 novembre*\n- Pagamento in `due rate`\n\n| Voce | Importo |\n|---|---|\n| Sito web | 1.200 euro |\n| Assistenza | 300 euro |\n\nPer dettagli vedi [la scheda](https://esempio.it/scheda).";
        assert_eq!(
            markdown_to_plain(md),
            "Riepilogo del preventivo\n\nEcco i punti importanti:\n\n- Consegna entro il 15 novembre\n- Pagamento in due rate\n\nVoce           Importo\nSito web    1.200 euro\nAssistenza    300 euro\n\nPer dettagli vedi la scheda (https://esempio.it/scheda)."
        );
    }

    #[test]
    fn code_blocks_keep_their_symbols_and_indentation() {
        let md = "Esempio:\n\n```rust\nlet a = x * y; // **not bold**\nif a_b > 2 { }\n```\n\nFine.";
        assert_eq!(markdown_to_plain(md), "Esempio:\n\nlet a = x * y; // **not bold**\nif a_b > 2 { }\n\nFine.");
        // the first line keeps its indent and two blank lines inside code stay two
        assert_eq!(markdown_to_plain("```python\n    def f(self):\n        return 1\n\n\n    def g(self): pass\n```"), "    def f(self):\n        return 1\n\n\n    def g(self): pass");
        assert_eq!(markdown_to_plain("    Caro cliente,\n\n\n\nsaluti"), "    Caro cliente,\n\nsaluti");
    }

    #[test]
    fn marks_without_a_partner_and_names_stay_as_written() {
        for t in [
            "2 * 3 = 6, a*b, snake_case_name e 5 > 3",
            "Modifica __init__.py e __main__.py",
            "Apri _config.yml e la cartella _layouts",
            "https://example.com/_next/static e https://it.wikipedia.org/wiki/Rust_(programming_language)",
            "2**10 = 1024 e x**2",
            "Elimina i file *.tmp",
            "Prezzo* 100 euro",
            ";-* e ^_^ e .*\\.txt$",
            "my_file_name.txt, #tag, +39 333, -5 gradi, [ ], [x], [1]",
            "C:\\dir\\file e \\\\server\\share",
            "3*4*5",
        ] {
            assert_eq!(markdown_to_plain(t), t, "{t}");
        }
    }

    #[test]
    fn paired_marks_go() {
        // underscores are never emphasis: they are names far more often
        assert_eq!(markdown_to_plain("_corsivo_ e __grassetto__ e ~~barrato~~ e ***entrambi***"), "_corsivo_ e __grassetto__ e barrato e entrambi");
        assert_eq!(markdown_to_plain("Il metodo __init__ e if __name__ == '__main__':"), "Il metodo __init__ e if __name__ == '__main__':");
        assert_eq!(markdown_to_plain("\\*non enfasi\\* e \\# non titolo e \\_x\\_"), "*non enfasi* e # non titolo e _x_");
        assert_eq!(markdown_to_plain("*Importi indicativi. Trattamento fiscale da definire.*"), "Importi indicativi. Trattamento fiscale da definire.");
        assert_eq!(markdown_to_plain("Totale *(IVA esclusa)* e _\"citato\"_, poi **(nota)**."), "Totale (IVA esclusa) e _\"citato\"_, poi (nota).");
        // emphasis around a path or an address: the marks go, the path stays
        assert_eq!(markdown_to_plain("Apri **C:\\temp** e **https://kotodama.app** e *dir/*"), "Apri C:\\temp e https://kotodama.app e dir/");
        // inline code is read from left to right: a backslash before the closing backtick is code
        assert_eq!(markdown_to_plain("`C:\\` e `C:\\Users\\` e D:\\"), "C:\\ e C:\\Users\\ e D:\\");
        assert_eq!(markdown_to_plain("il separatore e' `\\` su Windows"), "il separatore e' \\ su Windows");
        assert_eq!(markdown_to_plain("Usa `\\\\server\\share\\` qui"), "Usa \\\\server\\share\\ qui");
        // code that holds backticks: a run of N closes only with a run of N
        assert_eq!(markdown_to_plain("`` echo `date` `` e ``a ` b``"), "echo `date` e a ` b");
        assert_eq!(markdown_to_plain("```` ` ````"), "`");
        assert_eq!(markdown_to_plain("x `` y senza chiusura e `z`"), "x `` y senza chiusura e z");
        // three backticks with more backticks after them on the same line are code, not a fence
        assert_eq!(markdown_to_plain("``` inline triple ``` e **x**"), "inline triple e x");
        // emphasis around code pairs: the code is kept, the marks go
        assert_eq!(markdown_to_plain("**`npm install`** e **Usa `git push` ora** e *nota `x` qui* e ~~`old`~~"), "npm install e Usa git push ora e nota x qui e old");
        assert_eq!(markdown_to_plain("1. Esegui **`npm run dev`** e attendi"), "1. Esegui npm run dev e attendi");
        assert_eq!(markdown_to_plain("[`serde`](https://docs.rs/serde) e [vedi `x` qui](https://a.it)"), "serde (https://docs.rs/serde) e vedi x qui (https://a.it)");
        // two different spans never get mixed up, even inside a link; a private-use character in the text is safe
        assert_eq!(markdown_to_plain("[`npm`](`yarn`) poi `git` fine"), "npm (yarn) poi git fine");
        assert_eq!(markdown_to_plain("[`a`](`b`) e `c`"), "a (b) e c");
        assert_eq!(markdown_to_plain("\u{E000} `a` `b`"), "\u{E000} a b");
        // a line with an info string does not close a fence
        assert_eq!(markdown_to_plain("```\nuno\n```python\ndue\n```\nfuori **x**"), "uno\n```python\ndue\nfuori x");
        // a path ending with a backslash, with and without emphasis around it
        assert_eq!(markdown_to_plain("**C:\\** e **C:\\Program Files\\**"), "C:\\ e C:\\Program Files\\");
        // table cells with code and backticks
        assert_eq!(markdown_to_plain("| cmd | nota |\n|---|---|\n| `` a`b `` | **`x`** |"), "cmd  nota\na`b  x");
        assert_eq!(markdown_to_plain("**grassetto con *corsivo* dentro** e `a_b` e **x**"), "grassetto con corsivo dentro e a_b e x");
        assert_eq!(markdown_to_plain("x * (y + z) * 2"), "x * (y + z) * 2");
        // a backtick the page reading escaped is a plain backtick, and what is between is not code
        assert_eq!(markdown_to_plain("\\`a | b\\` e **x**"), "`a | b` e x");
        // an unpaired backtick stays, the rest of the line is not treated as code
        assert_eq!(markdown_to_plain("un ` isolato e **x**"), "un ` isolato e x");
    }

    #[test]
    fn headings_lists_rules_and_quotes() {
        let md = "# Titolo\n## Sotto\n* uno\n+ due\n  - annidato\n\t- con tab\n1. primo\n2. secondo\n---\n> citazione\n> > nidificata";
        assert_eq!(markdown_to_plain(md), "Titolo\nSotto\n- uno\n- due\n  - annidato\n\t- con tab\n1. primo\n2. secondo\n\n> citazione\n> > nidificata");
    }

    #[test]
    fn links_and_images() {
        assert_eq!(markdown_to_plain("[https://a.it](https://a.it) e [sito](https://b.it) e ![logo](https://c.it/l.png)"), "https://a.it e sito (https://b.it) e logo (https://c.it/l.png)");
        assert_eq!(markdown_to_plain("[non finito](https://x"), "[non finito](https://x");
        assert_eq!(markdown_to_plain("[wiki](https://it.wikipedia.org/wiki/Rust_(linguaggio)) e [doc](https://a.it/_private/x)"), "wiki (https://it.wikipedia.org/wiki/Rust_(linguaggio)) e doc (https://a.it/_private/x)");
    }

    #[test]
    fn tables() {
        // text columns are left aligned
        assert_eq!(markdown_to_plain("| Nome | Ruolo |\n|:--|:--|\n| Anna | Capo |\n| Bo | Dev |"), "Nome  Ruolo\nAnna  Capo\nBo    Dev");
        // empty cells, a row that is short, a bar inside code or escaped
        assert_eq!(markdown_to_plain("| a | b | c |\n|---|---|---|\n| 1 | | 3 |\n| 4 |"), "a  b  c\n1     3\n4");
        assert_eq!(markdown_to_plain("| cmd | nota |\n|---|---|\n| `a | b` | x \\| y |"), "cmd    nota\na | b  x | y");
        // a data row of dashes is not the separator; lines with a bar that are not a table are text
        assert_eq!(markdown_to_plain("| a | b |\n|---|---|\n| - | - |"), "a  b\n-  -");
        assert_eq!(markdown_to_plain("|x| e' il valore assoluto"), "|x| e' il valore assoluto");
        // shell globs and Windows paths come out as they went in
        for t in [
            "rm -rf ./*tmp*",
            "rm -rf /var/*cache*",
            "cp *.txt dir/*",
            "mv *.log /tmp/*",
            "find . -name *.py -o -name *.pyc*",
            "Cancella *_backup* e *.bak",
            "del C:\\temp\\*.tmp e del C:\\Users\\*.tmp",
            "C:\\Users\\_config\\file.txt",
            "D:\\progetti\\_vecchi\\*\\*.log",
            "\\\\server\\_share\\x",
        ] {
            assert_eq!(markdown_to_plain(t), t, "{t}");
        }
        // an address next to real emphasis: only the emphasis goes
        assert_eq!(markdown_to_plain("**vedi** https://a.it/_x e *questo*"), "vedi https://a.it/_x e questo");
        assert_eq!(markdown_to_plain("**input/output** e *and/or*"), "input/output e and/or");
        // a block opened with four backticks holds a three-backtick one
        assert_eq!(markdown_to_plain("````md\n```\ndentro\n```\n````\nfuori **x**"), "```\ndentro\n```\nfuori x");
        // wide characters count two columns
        assert_eq!(markdown_to_plain("| 名前 | 値 |\n|---|---|\n| 東京都 | 1 |\n| a | 22 |"), "名前    値\n東京都   1\na       22");
    }

    #[test]
    fn odd_input_never_panics() {
        for t in ["", "|", "||", "| a", "```", "[", "](", "**", "\\", "`", "# ", ">", "|---|", "é*日*é", "![", "[a](", "_", "~~", "a_", "_a", "*\n|\n|-|"] {
            let _ = markdown_to_plain(t);
        }
    }
}

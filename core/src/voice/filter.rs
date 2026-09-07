//! Disfluency filter for transcribed speech.
//!
//! ASR faithfully transcribes fillers ("嗯这个这个我们是吧…"); left in, they
//! waste tokens and dilute the model's attention. The rules here only remove
//! what is safely removable — a filler word is kept whenever it could be a
//! demonstrative ("这个方案不行" must survive). Anything subtler is left for
//! the chat model, which is told the message came from voice input.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterLevel {
    /// Return the transcript untouched.
    Off,
    /// Remove single-char interjections and immediately repeated filler words.
    Standard,
    /// Also remove lone filler words not followed by a content word.
    Aggressive,
}

impl FilterLevel {
    pub fn from_preference(value: Option<&str>) -> Result<Self, String> {
        match value {
            None | Some("standard") => Ok(FilterLevel::Standard),
            Some("off") => Ok(FilterLevel::Off),
            Some("aggressive") => Ok(FilterLevel::Aggressive),
            Some(other) => Err(format!("unknown voice filter level `{other}`")),
        }
    }
}

/// Single-char interjections: never content words, removable at any position.
const FILLER_CHARS: &[char] = &['嗯', '呃', '哦', '诶', '哎', '唉', '哇', '啊'];

/// Multi-char fillers, longest first so greedy matching does not split them.
/// Whether one is removed depends on context: repeated or trailing → filler,
/// followed by a content word → possibly a demonstrative, keep.
const FILLER_WORDS: &[&str] = &[
    "你知道吧",
    "怎么说呢",
    "就是说",
    "然后呢",
    "这个",
    "那个",
    "是吧",
    "对吧",
    "然后",
];

#[derive(Debug, Clone, PartialEq)]
enum Token {
    /// A filler word candidate (single-char flag distinguishes interjections).
    Filler {
        text: String,
        single: bool,
    },
    Word(String),
    Punct(String),
}

pub fn clean(text: &str, level: FilterLevel) -> String {
    if level == FilterLevel::Off {
        return text.trim().to_string();
    }

    let tokens = tokenize(text);
    let mut kept: Vec<Token> = Vec::new();
    let mut i = 0;

    while i < tokens.len() {
        if !matches!(tokens[i], Token::Filler { .. }) {
            kept.push(tokens[i].clone());
            i += 1;
            continue;
        }

        // Consume the whole run of consecutive fillers, then decide.
        let start = i;
        while i < tokens.len() && matches!(tokens[i], Token::Filler { .. }) {
            i += 1;
        }
        let run = &tokens[start..i];
        let next_is_word = matches!(tokens.get(i), Some(Token::Word(_)));

        match level {
            FilterLevel::Off => unreachable!(),
            FilterLevel::Standard => {
                // Interjections go; a multi-char filler goes only when the run
                // repeats it back-to-back ("这个这个"). A lone one is kept even
                // before punctuation — better to under-delete without a
                // confirmation step in front of the send.
                let mut prev: Option<&str> = None;
                for t in run {
                    let Token::Filler { text, single } = t else {
                        unreachable!()
                    };
                    if *single {
                        prev = None;
                        continue;
                    }
                    if prev == Some(text.as_str()) {
                        // Drop both halves of the repetition.
                        if let Some(Token::Filler { text: last, .. }) = kept.last()
                            && last == text
                        {
                            kept.pop();
                        }
                        continue;
                    }
                    prev = Some(text);
                    kept.push(t.clone());
                }
            }
            FilterLevel::Aggressive => {
                // Keep only a lone multi-char filler right before a content
                // word (likely a demonstrative); everything else in the run is
                // disfluency.
                if run.len() == 1
                    && next_is_word
                    && let Token::Filler { single: false, .. } = &run[0]
                {
                    kept.push(run[0].clone());
                }
            }
        }
    }

    render(&kept)
}

fn tokenize(text: &str) -> Vec<Token> {
    let chars: Vec<char> = text.chars().collect();
    let mut tokens = Vec::new();
    let mut i = 0;

    'outer: while i < chars.len() {
        let c = chars[i];

        if c.is_whitespace() {
            i += 1;
            continue;
        }
        if is_punct(c) {
            tokens.push(Token::Punct(c.to_string()));
            i += 1;
            continue;
        }
        if FILLER_CHARS.contains(&c) {
            tokens.push(Token::Filler {
                text: c.to_string(),
                single: true,
            });
            i += 1;
            continue;
        }
        // Runs of ASCII stay one token so "base url" cannot collapse into
        // "baseurl" when whitespace is dropped.
        if c.is_ascii_alphanumeric() {
            let start = i;
            while i < chars.len() && chars[i].is_ascii_alphanumeric() {
                i += 1;
            }
            tokens.push(Token::Word(chars[start..i].iter().collect()));
            continue;
        }
        for w in FILLER_WORDS {
            let wc: Vec<char> = w.chars().collect();
            if i + wc.len() <= chars.len() && chars[i..i + wc.len()] == wc[..] {
                tokens.push(Token::Filler {
                    text: (*w).to_string(),
                    single: false,
                });
                i += wc.len();
                continue 'outer;
            }
        }
        tokens.push(Token::Word(c.to_string()));
        i += 1;
    }

    tokens
}

fn is_punct(c: char) -> bool {
    matches!(
        c,
        '，' | '。' | '？' | '！' | '、' | '；' | '：' | ',' | '.' | '?' | '!' | ';' | ':'
    )
}

fn render(tokens: &[Token]) -> String {
    let mut out = String::new();
    // Leading punctuation is dangling too, so start "after punctuation".
    let mut last_was_punct = true;

    for t in tokens {
        let text = match t {
            Token::Punct(p) => {
                if !last_was_punct {
                    out.push_str(p);
                    last_was_punct = true;
                }
                continue;
            }
            Token::Word(w) => w,
            Token::Filler { text, .. } => text,
        };
        let prev_ascii = out.chars().last().is_some_and(|c| c.is_ascii_alphanumeric());
        let curr_ascii = text.starts_with(|c: char| c.is_ascii_alphanumeric());
        if prev_ascii && curr_ascii {
            out.push(' ');
        }
        out.push_str(text);
        last_was_punct = false;
    }

    out.trim_end_matches(is_punct).trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preference_is_a_closed_contract() {
        assert_eq!(FilterLevel::from_preference(None).unwrap(), FilterLevel::Standard);
        assert_eq!(FilterLevel::from_preference(Some("off")).unwrap(), FilterLevel::Off);
        assert_eq!(
            FilterLevel::from_preference(Some("aggressive")).unwrap(),
            FilterLevel::Aggressive
        );
        assert!(FilterLevel::from_preference(Some("future")).is_err());
        assert!(FilterLevel::from_preference(Some(" Standard ")).is_err());
    }

    #[test]
    fn off_returns_input_trimmed() {
        assert_eq!(clean(" 嗯这个 ", FilterLevel::Off), "嗯这个");
    }

    #[test]
    fn standard_drops_interjections_keeps_lone_fillers() {
        assert_eq!(
            clean("嗯这个方案不行，那个 API 有问题啊", FilterLevel::Standard),
            "这个方案不行，那个API有问题",
        );
    }

    #[test]
    fn standard_drops_repeated_fillers() {
        assert_eq!(clean("这个这个方案不行", FilterLevel::Standard), "方案不行");
    }

    #[test]
    fn aggressive_keeps_demonstrative_before_content_word() {
        assert_eq!(
            clean("这个方案不行，那个 API 有问题", FilterLevel::Aggressive),
            "这个方案不行，那个API有问题",
        );
    }

    #[test]
    fn aggressive_drops_fillers_and_keeps_ascii_spacing() {
        assert_eq!(
            clean(
                "嗯这个我们这个是吧就是说这个 provider 的 API 啊要改一下这个 base url 对吧",
                FilterLevel::Aggressive,
            ),
            "我们provider的API要改一下这个base url",
        );
    }

    #[test]
    fn aggressive_pure_disfluency_collapses_to_almost_nothing() {
        // The pathological all-filler clip; caller must treat "" / near-"" as
        // not worth sending.
        let cleaned = clean(
            "哇哎这个这个这个这个我们这个这个啊啊这个是吧啊这个这个啊啊这个",
            FilterLevel::Aggressive,
        );
        assert_eq!(cleaned, "我们");
    }

    #[test]
    fn empty_input_stays_empty() {
        assert_eq!(clean("", FilterLevel::Standard), "");
        assert_eq!(clean("嗯嗯啊", FilterLevel::Standard), "");
    }

    #[test]
    fn dangling_punctuation_is_cleaned() {
        assert_eq!(clean("嗯，走吧。", FilterLevel::Standard), "走吧");
    }

    #[test]
    fn english_words_keep_their_spaces() {
        assert_eq!(
            clean("the base url 是吧就是说不对", FilterLevel::Aggressive),
            "the base url不对",
        );
    }
}

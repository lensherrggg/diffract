use std::sync::OnceLock;

use jieba_rs::Jieba;

#[derive(Debug, Clone)]
pub struct TextFeatures {
    grams: Vec<u64>,
    words: Vec<u64>,
}

impl TextFeatures {
    pub fn from_normalized(text: &str) -> Self {
        let chars: Vec<char> = text.chars().collect();
        let mut grams = Vec::new();
        if chars.len() < 3 {
            if !chars.is_empty() {
                grams.push(hash_chars(&chars));
            }
        } else {
            for window in chars.windows(3) {
                grams.push(hash_chars(window));
            }
        }
        grams.sort_unstable();
        grams.dedup();

        let mut words = Vec::new();
        for token in jieba().cut(text, false) {
            if token.word.chars().any(|ch| ch.is_alphanumeric()) {
                words.push(hash_str(token.word));
            }
        }
        words.sort_unstable();
        words.dedup();

        Self { grams, words }
    }
}

fn jieba() -> &'static Jieba {
    static JIEBA: OnceLock<Jieba> = OnceLock::new();
    JIEBA.get_or_init(Jieba::new)
}

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

fn hash_str(value: &str) -> u64 {
    let mut hash = FNV_OFFSET;
    for byte in value.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

fn hash_chars(chars: &[char]) -> u64 {
    let mut hash = FNV_OFFSET;
    for ch in chars {
        let mut buf = [0u8; 4];
        for byte in ch.encode_utf8(&mut buf).as_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(FNV_PRIME);
        }
    }
    hash
}

fn jaccard(a: &[u64], b: &[u64]) -> f64 {
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    let (mut i, mut j, mut intersection) = (0usize, 0usize, 0usize);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Equal => {
                intersection += 1;
                i += 1;
                j += 1;
            }
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
        }
    }
    let union = a.len() + b.len() - intersection;
    if union == 0 {
        1.0
    } else {
        intersection as f64 / union as f64
    }
}

/// 字符 3-gram Jaccard 与中文分词词重合率各占一半。
pub fn text_similarity(a: &TextFeatures, b: &TextFeatures) -> f64 {
    let gram = jaccard(&a.grams, &b.grams);
    let word = jaccard(&a.words, &b.words);
    0.5 * gram + 0.5 * word
}

pub fn cosine(a: &[f32], b: &[f32]) -> f64 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0f64;
    let mut na = 0.0f64;
    let mut nb = 0.0f64;
    for (x, y) in a.iter().zip(b.iter()) {
        let x = f64::from(*x);
        let y = f64::from(*y);
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        (dot / (na.sqrt() * nb.sqrt())).clamp(-1.0, 1.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_text_is_one() {
        let a = TextFeatures::from_normalized("公司应当建立安全管理制度。");
        let b = TextFeatures::from_normalized("公司应当建立安全管理制度。");
        assert!((text_similarity(&a, &b) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn paraphrases_score_lower_than_near_copy() {
        let base = TextFeatures::from_normalized("各部门每季度开展一次安全自查并报送结果。");
        let near = TextFeatures::from_normalized("各部门每季度开展一次安全自查并报送检查结果。");
        let far = TextFeatures::from_normalized("财务部门负责编制年度预算报告。");
        let near_score = text_similarity(&base, &near);
        let far_score = text_similarity(&base, &far);
        assert!(near_score > 0.7, "near={near_score}");
        assert!(far_score < 0.2, "far={far_score}");
    }

    #[test]
    fn cosine_is_scale_invariant() {
        assert!((cosine(&[1.0, 0.0], &[2.0, 0.0]) - 1.0).abs() < 1e-9);
        assert!(cosine(&[1.0, 0.0], &[0.0, 1.0]).abs() < 1e-9);
    }
}

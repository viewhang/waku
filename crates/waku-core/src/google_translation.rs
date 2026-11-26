//! Stateless requests to Google Translate's unauthenticated web endpoint

use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use reqwest::Client;
use serde::Deserialize;
use url::Url;
use waku_protocol::{GOOGLE_TRANSLATION_MAX_CHARACTERS, TranslationLanguage};

const ENDPOINT: &str = "https://translate.googleapis.com/translate_a/t";
// Bound the escaped query because Google rejects very long request URLs
const MAX_ENCODED_TEXT_BYTES: usize = 6_000;

pub(crate) fn translate(
    text: &str,
    source: Option<TranslationLanguage>,
    target: TranslationLanguage,
) -> Result<String> {
    if text.trim().is_empty() {
        bail!("Enter text to translate");
    }
    if text
        .chars()
        .take(GOOGLE_TRANSLATION_MAX_CHARACTERS + 1)
        .count()
        > GOOGLE_TRANSLATION_MAX_CHARACTERS
    {
        bail!("Google translation accepts up to {GOOGLE_TRANSLATION_MAX_CHARACTERS} characters");
    }
    if source == Some(target) {
        bail!("Choose different source and target languages");
    }
    let client = Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        tokio::time::timeout(
            Duration::from_secs(30),
            translate_chunks(&client, ENDPOINT, text, source, target),
        )
        .await
        .context("Google Translate timed out")?
    })
}

/// Prefer a nearby word boundary while keeping every URL within the same budget
fn chunk_end(text: &str) -> usize {
    let mut encoded_bytes = 0;
    for (index, character) in text.char_indices() {
        encoded_bytes += if character.is_ascii_alphanumeric()
            || matches!(character, ' ' | '*' | '-' | '.' | '_')
        {
            1
        } else {
            character.len_utf8() * 3
        };
        if encoded_bytes > MAX_ENCODED_TEXT_BYTES {
            return text[..index]
                .rfind(char::is_whitespace)
                .filter(|boundary| *boundary > index / 2)
                .unwrap_or(index);
        }
    }
    text.len()
}

async fn translate_chunks(
    client: &Client,
    endpoint: &str,
    mut text: &str,
    source: Option<TranslationLanguage>,
    target: TranslationLanguage,
) -> Result<String> {
    let source = source.map_or("auto", language_code);
    let target = language_code(target);
    let mut translation = String::new();
    while !text.is_empty() {
        let (chunk, remaining) = text.split_at(chunk_end(text));
        text = remaining;

        // Keep whitespace at the edges of each piece when joining translations
        let start = chunk.trim_start();
        translation.push_str(&chunk[..chunk.len() - start.len()]);
        let body = start.trim_end();
        if !body.is_empty() {
            translation.push_str(&fetch_translation(client, endpoint, body, source, target).await?);
            translation.push_str(&start[body.len()..]);
        }
    }
    // Trim the joined result so separators between chunks are preserved
    Ok(translation.trim().to_owned())
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Translation {
    Explicit(String),
    Detected(String, serde::de::IgnoredAny),
}

async fn fetch_translation(
    client: &Client,
    endpoint: &str,
    text: &str,
    source: &str,
    target: &str,
) -> Result<String> {
    let mut url = Url::parse(endpoint)?;
    url.query_pairs_mut().extend_pairs([
        ("client", "dict-chrome-ex"),
        ("sl", source),
        ("tl", target),
        ("q", text),
    ]);
    let response = client
        .get(url)
        .send()
        .await
        // The URL contains the source text, so omit it from daemon errors
        .map_err(reqwest::Error::without_url)
        .context("Could not reach Google Translate")?;
    let status = response.status();
    if !status.is_success() {
        bail!("Google Translate returned HTTP {status}");
    }
    let [result] = response
        .json::<[Translation; 1]>()
        .await
        .map_err(reqwest::Error::without_url)
        .context("Invalid Google Translate response")?;
    let (Translation::Explicit(text) | Translation::Detected(text, _)) = result;
    if text.trim().is_empty() {
        bail!("Google Translate returned no translation");
    }
    Ok(text)
}

fn language_code(language: TranslationLanguage) -> &'static str {
    use TranslationLanguage::*;
    match language {
        SimplifiedChinese => "zh-CN",
        TraditionalChinese => "zh-TW",
        English => "en",
        Japanese => "ja",
        Korean => "ko",
        French => "fr",
        German => "de",
        Spanish => "es",
        Portuguese => "pt",
        Italian => "it",
        Russian => "ru",
        Arabic => "ar",
        Thai => "th",
        Vietnamese => "vi",
    }
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead as _, BufReader, Write as _};
    use std::net::TcpListener;

    use super::*;

    #[test]
    fn google_translation_accepts_both_web_response_shapes_without_credentials() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let endpoint = format!("http://{}/translate_a/t", listener.local_addr().unwrap());
        let provider = std::thread::spawn(move || {
            for (source, body) in [
                ("auto", r#"[[" \n你好！\n世界\n ","en"]]"#),
                ("en", r#"[" \n你好！\n世界\n "]"#),
            ] {
                let (mut socket, _) = listener.accept().unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut reader = BufReader::new(&mut socket);
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                assert_eq!(
                    line,
                    format!(
                        "GET /translate_a/t?client=dict-chrome-ex&sl={source}&tl=zh-CN&q=Hello%21%0Aworld HTTP/1.1\r\n"
                    ),
                );
                for line in reader.lines() {
                    let line = line.unwrap();
                    if line.is_empty() {
                        break;
                    }
                    let header = line.split(':').next().unwrap().to_ascii_lowercase();
                    assert!(!matches!(
                        header.as_str(),
                        "authorization" | "cookie" | "x-goog-api-key"
                    ));
                }
                write!(socket,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()).unwrap();
            }
        });
        let client = Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        for source in [None, Some(TranslationLanguage::English)] {
            let result = runtime
                .block_on(translate_chunks(
                    &client,
                    &endpoint,
                    "  Hello!\nworld\n",
                    source,
                    TranslationLanguage::SimplifiedChinese,
                ))
                .unwrap();
            assert_eq!(result, "你好！\n世界");
        }
        provider.join().unwrap();
    }

    #[test]
    fn chunks_bound_the_percent_encoded_size_without_cutting_utf8() {
        let text = format!("{}\n{}", "祝".repeat(640), "你好。".repeat(150));
        assert_eq!(&text[chunk_end(&text)..chunk_end(&text) + 1], "\n");

        let mut remaining = text.as_str();
        while !remaining.is_empty() {
            let (chunk, rest) = remaining.split_at(chunk_end(remaining));
            let encoded =
                url::form_urlencoded::byte_serialize(chunk.as_bytes()).collect::<String>();
            assert!(!chunk.is_empty());
            assert!(encoded.len() <= MAX_ENCODED_TEXT_BYTES);
            remaining = rest;
        }
    }
}

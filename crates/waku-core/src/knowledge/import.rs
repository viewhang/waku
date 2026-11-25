//! Import public web pages without giving a model access to daemon-host services

use anyhow::{Context, Result, bail, ensure};
use dom_query::Document;
use reqwest::blocking::Client;
use serde_json::json;
use std::{
    collections::HashMap,
    io::Read,
    net::{IpAddr, ToSocketAddrs},
    time::Duration,
};
use url::Url;
use waku_protocol::knowledge::ImportOptions;

/// Pin each request to validated public addresses, including on every redirect and image fetch
fn public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v) => {
            let [a, b, c, _] = v.octets();
            !(v.is_private()
                || v.is_loopback()
                || v.is_link_local()
                || v.is_documentation()
                || a == 0
                || a >= 224
                || (a == 100 && (64..128).contains(&b))
                || (a == 198 && (b == 18 || b == 19))
                || (a == 192 && b == 0 && (c == 0 || c == 2)))
        }
        IpAddr::V6(v) => {
            v.octets()[0] & 0xe0 == 0x20
                && v.segments()[..2] != [0x2001, 0xdb8]
                && v.segments()[0] != 0x2002
                && !(v.segments()[0] == 0x2001 && v.segments()[1] < 0x200)
        }
    }
}

/// Authenticate a DNS proxy’s synthetic address using HTTPS for the requested hostname on port 443
fn allowed_addresses(url: &Url, addresses: &[std::net::SocketAddr]) -> bool {
    if addresses.is_empty() {
        return false;
    }
    if addresses.iter().all(|a| public(a.ip())) {
        return true;
    }
    let Some(url::Host::Domain(domain)) = url.host() else {
        return false;
    };
    let domain = domain.trim_end_matches('.');
    url.scheme() == "https"
        && url.port_or_known_default() == Some(443)
        && domain.contains('.')
        && !matches!(
            domain.rsplit('.').next(),
            Some(
                "localhost"
                    | "local"
                    | "arpa"
                    | "internal"
                    | "lan"
                    | "home"
                    | "test"
                    | "invalid"
                    | "onion"
            )
        )
        && addresses
            .iter()
            .all(|a| matches!(a.ip(),IpAddr::V4(v) if matches!(v.octets(),[198,18|19,_,_])))
}

fn fetch(mut url: Url, limit: usize) -> Result<(Url, String, Vec<u8>)> {
    for _ in 0..=5 {
        ensure!(
            matches!(url.scheme(), "http" | "https")
                && url.username().is_empty()
                && url.password().is_none(),
            "Expected a public HTTP(S) URL"
        );
        let host = url
            .host_str()
            .context("Missing URL host")?
            .trim_matches(['[', ']']);
        let addresses: Vec<_> = (host, url.port_or_known_default().context("Missing port")?)
            .to_socket_addrs()?
            .collect();
        ensure!(
            allowed_addresses(&url, &addresses),
            "Only public web addresses can be imported"
        );
        let client = Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(15))
            .resolve_to_addrs(host, &addresses)
            .build()?;
        let response = client.get(url.clone()).send()?;
        if response.status().is_redirection() {
            url = url.join(
                response
                    .headers()
                    .get("location")
                    .context("Redirect missing location")?
                    .to_str()?,
            )?;
            continue;
        }
        let response = response.error_for_status()?;
        let mime = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .split(';')
            .next()
            .unwrap_or("")
            .to_lowercase();
        let mut bytes = Vec::new();
        response.take(limit as u64 + 1).read_to_end(&mut bytes)?;
        ensure!(bytes.len() <= limit, "Import exceeds size limit");
        return Ok((url, mime, bytes));
    }
    bail!("Too many redirects")
}

pub(super) struct Article {
    pub text: String,
    pub images: Vec<(String, Vec<u8>)>,
}

pub(super) fn article(url: &str, opts: &ImportOptions) -> Result<Article> {
    let (url, mime, html) = fetch(Url::parse(url)?, 4 * 1024 * 1024)?;
    article_from_response(&url, &mime, html, opts)
}

/// Convert the fetched response using its final URL as the base for links and source metadata
fn article_from_response(
    url: &Url,
    mime: &str,
    bytes: Vec<u8>,
    opts: &ImportOptions,
) -> Result<Article> {
    let html = String::from_utf8(bytes).context("Expected UTF-8 article")?;
    if matches!(mime, "text/plain" | "text/markdown") {
        return Ok(Article {
            text: article_text(html, url, None, opts)?,
            images: vec![],
        });
    }
    ensure!(
        matches!(mime, "text/html" | "application/xhtml+xml" | ""),
        "Expected an HTML article"
    );
    let doc = Document::from(html);
    let title = doc.select("title").text().trim().to_owned();
    doc.select("script,style,template,noscript,iframe").remove();
    if opts.clean_content {
        doc.select("nav,footer,aside,form,[role=navigation],.advertisement")
            .remove();
    }
    let content = if opts.clean_content {
        doc.try_select("[itemprop=articleBody],.entry-content,.post-content,#cnblogs_post_body")
            .or_else(|| doc.try_select("article"))
            .or_else(|| doc.try_select("main"))
    } else {
        None
    }
    .unwrap_or_else(|| doc.select("body"))
    .first();
    for link in content.select("a[href]").iter() {
        if let Some(raw) = link.attr("href")
            && let Ok(absolute) = url.join(&raw)
        {
            link.set_attr("href", absolute.as_str());
        }
    }
    let mut images = Vec::new();
    let mut saved = HashMap::<String, String>::new();
    let mut remaining = 16 * 1024 * 1024;
    for img in content.select("img").iter() {
        if !opts.include_images {
            img.remove();
            continue;
        }
        let Some(absolute) = img.attr("src").and_then(|raw| url.join(&raw).ok()) else {
            img.remove();
            continue;
        };
        if let Some(name) = saved.get(absolute.as_str()) {
            img.set_attr("src", name);
            continue;
        }
        ensure!(
            images.len() < 32,
            "An article can contain at most 32 imported images"
        );
        let (_, mime, data) = fetch(absolute.clone(), remaining.min(4 * 1024 * 1024))?;
        let ext = match mime.as_str() {
            "image/png" => "png",
            "image/jpeg" => "jpg",
            "image/gif" => "gif",
            "image/webp" => "webp",
            "image/avif" => "avif",
            _ => {
                img.remove();
                continue;
            }
        };
        remaining -= data.len();
        let name = format!("attachments/{}.{}", uuid::Uuid::new_v4(), ext);
        img.set_attr("src", &name);
        saved.insert(absolute.to_string(), name.clone());
        images.push((name, data));
    }
    let text = content
        .nodes()
        .first()
        .context("Article has no body")?
        .md(None)
        .to_string();
    ensure!(!text.trim().is_empty(), "The page contains no article text");
    Ok(Article {
        text: article_text(text, url, Some(title.as_ref()), opts)?,
        images,
    })
}

fn article_text(
    text: String,
    url: &Url,
    title: Option<&str>,
    opts: &ImportOptions,
) -> Result<String> {
    if !opts.add_metadata {
        return Ok(text);
    }
    let mut fields = json!({"source":url.as_str(),"imported_at":chrono::Utc::now().to_rfc3339()});
    if let Some(title) = title.filter(|s| !s.is_empty()) {
        fields["title"] = json!(title);
    }
    super::markdown::metadata(&text, fields.as_object().unwrap().clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A saved page contains its article body, resolved links and final source URL
    #[test]
    fn knowledge_import_converts_article_and_preserves_source() -> Result<()> {
        let url = Url::parse("https://example.com/articles/demo")?;
        let synthetic = ["198.18.0.1:443".parse()?];
        assert!(allowed_addresses(&url, &synthetic));
        for blocked in [
            "http://example.com",
            "https://198.18.0.1",
            "https://host.local",
            "https://example.com:8080",
        ] {
            assert!(!allowed_addresses(&Url::parse(blocked)?, &synthetic));
        }
        assert!(!allowed_addresses(&url, &["127.0.0.1:443".parse()?]));
        let html = r#"<html><title>Demo</title><body><nav>omit nav</nav><article><h2>Sample</h2><p>Read <a href="../notes">more</a></p><script>omit script</script></article></body></html>"#;
        let article = article_from_response(
            &url,
            "text/html",
            html.as_bytes().to_vec(),
            &ImportOptions::default(),
        )?;
        assert!(
            article.text.contains("## Sample")
                && article.text.contains("[more](https://example.com/notes)")
        );
        assert!(!article.text.contains("omit") && article.text.contains(url.as_str()));
        let plain = article_from_response(
            &url,
            "text/markdown",
            b"# Notes".to_vec(),
            &ImportOptions::default(),
        )?;
        assert!(plain.text.contains(url.as_str()) && plain.text.ends_with("# Notes"));
        assert!(super::article("http://127.0.0.1", &ImportOptions::default()).is_err());
        Ok(())
    }
}

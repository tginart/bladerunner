//! HTTP client for the local `web_search` tool.
//!
//! Purpose: This module sends one search query to the configured provider
//! (DuckDuckGo HTML, Exa or Perplexity) and returns a uniform result list.
//! Note: Codex native search does not use this module. The sampler sends it
//! as a hosted tool on the model request.

use super::types::WebSearchConfig;
use std::time::Duration;

const DUCKDUCKGO_URL: &str = "https://html.duckduckgo.com/html/";
const EXA_URL: &str = "https://api.exa.ai/search";
const PERPLEXITY_URL: &str = "https://api.perplexity.ai/search";
const MAX_RESULTS: usize = 10;
const MAX_SNIPPET_CHARS: usize = 1200;
const REQUEST_TIMEOUT_S: u64 = 30;
// html.duckduckgo.com returns an empty page to clients without a browser UA.
const BROWSER_USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 14_0) \
    AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0 Safari/537.36";

/// One search hit, normalized across providers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchHit {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

#[derive(Clone)]
enum Provider {
    DuckDuckGo,
    Exa { api_key: String },
    Perplexity { api_key: String },
}

/// Search client for one provider. Cheap to clone.
#[derive(Clone)]
pub struct WebSearchClient {
    http: reqwest::Client,
    provider: Provider,
    endpoint: String,
}

impl WebSearchClient {
    /// Create a client from an enabled config. `Disabled` gives an error.
    pub fn new(config: &WebSearchConfig) -> Result<Self, xai_tool_runtime::ToolError> {
        let (provider, endpoint) = match config {
            WebSearchConfig::Disabled => {
                return Err(tool_error("web search is disabled"));
            }
            WebSearchConfig::DuckDuckGo => (Provider::DuckDuckGo, DUCKDUCKGO_URL),
            WebSearchConfig::Exa { api_key } => (
                Provider::Exa {
                    api_key: api_key.clone(),
                },
                EXA_URL,
            ),
            WebSearchConfig::Perplexity { api_key } => (
                Provider::Perplexity {
                    api_key: api_key.clone(),
                },
                PERPLEXITY_URL,
            ),
        };
        let http = xai_grok_extra_ca::with_extra_root_certificates(
            reqwest::Client::builder()
                .timeout(Duration::from_secs(REQUEST_TIMEOUT_S))
                .user_agent(BROWSER_USER_AGENT),
        )
        .build()
        .map_err(|e| tool_error(format!("Failed to build HTTP client: {e}")))?;
        Ok(Self {
            http,
            provider,
            endpoint: endpoint.to_string(),
        })
    }

    /// Point the client at a different endpoint (tests use a mock server).
    #[cfg(test)]
    fn with_endpoint(mut self, endpoint: String) -> Self {
        self.endpoint = endpoint;
        self
    }

    /// Short provider label, e.g. for the tool output header.
    pub fn provider_name(&self) -> &'static str {
        match self.provider {
            Provider::DuckDuckGo => "DuckDuckGo",
            Provider::Exa { .. } => "Exa",
            Provider::Perplexity { .. } => "Perplexity",
        }
    }

    /// Run one query.
    ///
    /// Returns `(content, citations)`. `content` is a numbered list of hits
    /// for the model. `citations` holds the hit URLs in the same order.
    pub async fn search(
        &self,
        query: &str,
        allowed_domains: Option<Vec<String>>,
    ) -> Result<(String, Vec<String>), xai_tool_runtime::ToolError> {
        let domains = allowed_domains.unwrap_or_default();
        let hits = match &self.provider {
            Provider::DuckDuckGo => self.search_duckduckgo(query, &domains).await?,
            Provider::Exa { api_key } => self.search_exa(api_key, query, &domains).await?,
            Provider::Perplexity { api_key } => {
                self.search_perplexity(api_key, query, &domains).await?
            }
        };
        let citations = hits.iter().map(|hit| hit.url.clone()).collect();
        Ok((format_hits(&hits), citations))
    }

    async fn search_duckduckgo(
        &self,
        query: &str,
        domains: &[String],
    ) -> Result<Vec<SearchHit>, xai_tool_runtime::ToolError> {
        let full_query = duckduckgo_query(query, domains);
        let response = self
            .http
            .get(&self.endpoint)
            .query(&[("q", full_query.as_str())])
            .send()
            .await
            .map_err(|e| tool_error(format!("DuckDuckGo request failed: {e}")))?;
        let html = read_success_body(response, "DuckDuckGo").await?;
        let hits = parse_duckduckgo_html(&html);
        if hits.is_empty() && html.contains("anomaly") {
            return Err(tool_error(
                "DuckDuckGo rejected the request as automated traffic. Try again later, \
                 or configure Exa or Perplexity with /web-search.",
            ));
        }
        Ok(filter_by_domains(hits, domains))
    }

    async fn search_exa(
        &self,
        api_key: &str,
        query: &str,
        domains: &[String],
    ) -> Result<Vec<SearchHit>, xai_tool_runtime::ToolError> {
        let mut body = serde_json::json!({
            "query": query,
            "numResults": MAX_RESULTS,
            "contents": { "text": { "maxCharacters": MAX_SNIPPET_CHARS } },
        });
        if !domains.is_empty() {
            body["includeDomains"] = serde_json::json!(domains);
        }
        let response = self
            .http
            .post(&self.endpoint)
            .header("x-api-key", api_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| tool_error(format!("Exa request failed: {e}")))?;
        let text = read_success_body(response, "Exa").await?;
        parse_results_json(&text, "text", "Exa")
    }

    async fn search_perplexity(
        &self,
        api_key: &str,
        query: &str,
        domains: &[String],
    ) -> Result<Vec<SearchHit>, xai_tool_runtime::ToolError> {
        let mut body = serde_json::json!({
            "query": query,
            "max_results": MAX_RESULTS,
        });
        if !domains.is_empty() {
            body["search_domain_filter"] = serde_json::json!(domains);
        }
        let response = self
            .http
            .post(&self.endpoint)
            .bearer_auth(api_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| tool_error(format!("Perplexity request failed: {e}")))?;
        let text = read_success_body(response, "Perplexity").await?;
        parse_results_json(&text, "snippet", "Perplexity")
    }
}

/// Append `site:` terms so DuckDuckGo restricts the search to `domains`.
fn duckduckgo_query(query: &str, domains: &[String]) -> String {
    if domains.is_empty() {
        return query.to_string();
    }
    let sites: Vec<String> = domains.iter().map(|d| format!("site:{d}")).collect();
    format!("{query} {}", sites.join(" OR "))
}

/*
Purpose: Extract the organic results from a html.duckduckgo.com page.
Input: The page HTML.
Output: The hits in page order, at most MAX_RESULTS.
Method:
    1. Find each `div.result` that is not an ad.
    2. Read the title and link from `a.result__a`.
    3. Decode the real URL from the `uddg` parameter of the redirect link.
    4. Read the snippet from `.result__snippet`.
*/
fn parse_duckduckgo_html(html: &str) -> Vec<SearchHit> {
    let document = scraper::Html::parse_document(html);
    let result_sel = scraper::Selector::parse("div.result").expect("valid selector");
    let link_sel = scraper::Selector::parse("a.result__a").expect("valid selector");
    let snippet_sel = scraper::Selector::parse(".result__snippet").expect("valid selector");
    let mut hits = Vec::new();
    for result in document.select(&result_sel) {
        if result.value().classes().any(|c| c == "result--ad") {
            continue;
        }
        let Some(link) = result.select(&link_sel).next() else {
            continue;
        };
        let Some(url) = link.value().attr("href").and_then(decode_duckduckgo_href) else {
            continue;
        };
        let snippet = result
            .select(&snippet_sel)
            .next()
            .map(|s| collapse_whitespace(&s.text().collect::<String>()))
            .unwrap_or_default();
        hits.push(SearchHit {
            title: collapse_whitespace(&link.text().collect::<String>()),
            url,
            snippet,
        });
        if hits.len() == MAX_RESULTS {
            break;
        }
    }
    hits
}

/// DuckDuckGo links go through `//duckduckgo.com/l/?uddg=<target>`.
fn decode_duckduckgo_href(href: &str) -> Option<String> {
    let absolute = if href.starts_with("//") {
        format!("https:{href}")
    } else {
        href.to_string()
    };
    let parsed = url::Url::parse(&absolute).ok()?;
    if parsed.host_str() == Some("duckduckgo.com") && parsed.path() == "/l/" {
        let (_, target) = parsed.query_pairs().find(|(k, _)| k == "uddg")?;
        return Some(target.into_owned());
    }
    matches!(parsed.scheme(), "http" | "https").then_some(absolute)
}

/// Drop hits whose host is not one of `domains` or a subdomain of one.
fn filter_by_domains(hits: Vec<SearchHit>, domains: &[String]) -> Vec<SearchHit> {
    if domains.is_empty() {
        return hits;
    }
    let mut kept = Vec::new();
    for hit in hits {
        let host = url::Url::parse(&hit.url)
            .ok()
            .and_then(|u| u.host_str().map(str::to_ascii_lowercase));
        let Some(host) = host else { continue };
        let is_allowed = domains.iter().any(|d| {
            let d = d.trim_start_matches("www.").to_ascii_lowercase();
            host == d || host.ends_with(&format!(".{d}"))
        });
        if is_allowed {
            kept.push(hit);
        }
    }
    kept
}

/// Parse `{"results": [{"title", "url", <snippet_field>}]}` (Exa, Perplexity).
fn parse_results_json(
    text: &str,
    snippet_field: &str,
    provider: &str,
) -> Result<Vec<SearchHit>, xai_tool_runtime::ToolError> {
    let value: serde_json::Value = serde_json::from_str(text)
        .map_err(|e| tool_error(format!("{provider} returned invalid JSON: {e}")))?;
    let results = value["results"]
        .as_array()
        .ok_or_else(|| tool_error(format!("{provider} response has no `results` array")))?;
    let mut hits = Vec::new();
    for result in results.iter().take(MAX_RESULTS) {
        let Some(url) = result["url"].as_str() else {
            continue;
        };
        hits.push(SearchHit {
            title: result["title"]
                .as_str()
                .unwrap_or_default()
                .trim()
                .to_string(),
            url: url.to_string(),
            snippet: collapse_whitespace(result[snippet_field].as_str().unwrap_or_default()),
        });
    }
    Ok(hits)
}

/// Render hits as the numbered list the model sees.
fn format_hits(hits: &[SearchHit]) -> String {
    if hits.is_empty() {
        return "No search results found.".to_string();
    }
    let mut out = String::new();
    for (i, hit) in hits.iter().enumerate() {
        let title = if hit.title.is_empty() {
            &hit.url
        } else {
            &hit.title
        };
        out.push_str(&format!("{}. {title}\n   URL: {}\n", i + 1, hit.url));
        if !hit.snippet.is_empty() {
            let snippet: String = hit.snippet.chars().take(MAX_SNIPPET_CHARS).collect();
            out.push_str(&format!("   {snippet}\n"));
        }
        out.push('\n');
    }
    out.trim_end().to_string()
}

async fn read_success_body(
    response: reqwest::Response,
    provider: &str,
) -> Result<String, xai_tool_runtime::ToolError> {
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|e| tool_error(format!("Failed to read {provider} response: {e}")))?;
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        return Err(xai_tool_runtime::ToolError::unauthorized(format!(
            "{provider} rejected the API key ({status}). Update it with /web-search."
        )));
    }
    if !status.is_success() {
        let preview: String = body.chars().take(500).collect();
        return Err(tool_error(format!(
            "{provider} returned {status}: {preview}"
        )));
    }
    Ok(body)
}

fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn tool_error(message: impl Into<String>) -> xai_tool_runtime::ToolError {
    xai_tool_runtime::ToolError::execution(
        xai_tool_protocol::ToolId::new("web_search").expect("valid tool id"),
        message.into(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_partial_json, header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const DDG_HTML: &str = r#"
        <div class="result results_links result--ad">
          <a class="result__a" href="https://ads.example/">Ad</a>
        </div>
        <div class="result results_links">
          <h2><a class="result__a"
            href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fdocs.rs%2Ftokio&amp;rut=abc">
            Tokio   docs</a></h2>
          <a class="result__snippet">An async
            runtime.</a>
        </div>
        <div class="result results_links">
          <a class="result__a" href="https://example.com/page">Example</a>
        </div>
    "#;

    fn client(config: WebSearchConfig, server: &MockServer) -> WebSearchClient {
        WebSearchClient::new(&config)
            .unwrap()
            .with_endpoint(format!("{}/search", server.uri()))
    }

    #[test]
    fn disabled_config_has_no_client() {
        assert!(WebSearchClient::new(&WebSearchConfig::Disabled).is_err());
    }

    #[test]
    fn parses_duckduckgo_results_and_skips_ads() {
        let hits = parse_duckduckgo_html(DDG_HTML);
        assert_eq!(
            hits,
            vec![
                SearchHit {
                    title: "Tokio docs".into(),
                    url: "https://docs.rs/tokio".into(),
                    snippet: "An async runtime.".into(),
                },
                SearchHit {
                    title: "Example".into(),
                    url: "https://example.com/page".into(),
                    snippet: String::new(),
                },
            ]
        );
    }

    #[test]
    fn domain_filter_keeps_subdomains_only() {
        let hits = parse_duckduckgo_html(DDG_HTML);
        let kept = filter_by_domains(hits, &["rs".into(), "www.example.com".into()]);
        let urls: Vec<_> = kept.iter().map(|h| h.url.as_str()).collect();
        assert_eq!(
            urls,
            vec!["https://docs.rs/tokio", "https://example.com/page"]
        );
        let kept = filter_by_domains(parse_duckduckgo_html(DDG_HTML), &["docs.rs".into()]);
        assert_eq!(kept.len(), 1);
    }

    #[test]
    fn duckduckgo_query_adds_site_terms() {
        assert_eq!(duckduckgo_query("q", &[]), "q");
        assert_eq!(
            duckduckgo_query("q", &["a.com".into(), "b.org".into()]),
            "q site:a.com OR site:b.org"
        );
    }

    #[test]
    fn format_hits_numbers_results() {
        let hits = vec![SearchHit {
            title: String::new(),
            url: "https://x.dev".into(),
            snippet: "s".into(),
        }];
        assert_eq!(
            format_hits(&hits),
            "1. https://x.dev\n   URL: https://x.dev\n   s"
        );
        assert_eq!(format_hits(&[]), "No search results found.");
    }

    #[tokio::test]
    async fn duckduckgo_search_end_to_end() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/search"))
            .and(query_param("q", "tokio"))
            .respond_with(ResponseTemplate::new(200).set_body_string(DDG_HTML))
            .mount(&server)
            .await;
        let (content, citations) = client(WebSearchConfig::DuckDuckGo, &server)
            .search("tokio", None)
            .await
            .unwrap();
        assert!(content.starts_with("1. Tokio docs\n   URL: https://docs.rs/tokio"));
        assert_eq!(
            citations,
            vec!["https://docs.rs/tokio", "https://example.com/page"]
        );
    }

    #[tokio::test]
    async fn duckduckgo_anomaly_page_is_an_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string("<form id=\"anomaly\">"))
            .mount(&server)
            .await;
        let err = client(WebSearchConfig::DuckDuckGo, &server)
            .search("q", None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("automated traffic"), "{err}");
    }

    #[tokio::test]
    async fn exa_sends_key_and_domains() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/search"))
            .and(header("x-api-key", "exa-key"))
            .and(body_partial_json(serde_json::json!({
                "query": "rust",
                "includeDomains": ["rust-lang.org"],
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "results": [
                    {"title": "Rust", "url": "https://rust-lang.org/", "text": "A language"},
                    {"title": "No url"}
                ]
            })))
            .mount(&server)
            .await;
        let config = WebSearchConfig::Exa {
            api_key: "exa-key".into(),
        };
        let (content, citations) = client(config, &server)
            .search("rust", Some(vec!["rust-lang.org".into()]))
            .await
            .unwrap();
        assert_eq!(citations, vec!["https://rust-lang.org/"]);
        assert!(content.contains("A language"));
    }

    #[tokio::test]
    async fn perplexity_sends_bearer_and_domain_filter() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/search"))
            .and(header("Authorization", "Bearer pplx-key"))
            .and(body_partial_json(serde_json::json!({
                "query": "news",
                "search_domain_filter": ["bbc.com"],
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "results": [{"title": "BBC", "url": "https://bbc.com/a", "snippet": "S"}]
            })))
            .mount(&server)
            .await;
        let config = WebSearchConfig::Perplexity {
            api_key: "pplx-key".into(),
        };
        let (content, citations) = client(config, &server)
            .search("news", Some(vec!["bbc.com".into()]))
            .await
            .unwrap();
        assert_eq!(citations, vec!["https://bbc.com/a"]);
        assert_eq!(content, "1. BBC\n   URL: https://bbc.com/a\n   S");
    }

    #[tokio::test]
    async fn rejected_key_is_unauthorized() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(401).set_body_string("bad key"))
            .mount(&server)
            .await;
        let config = WebSearchConfig::Exa {
            api_key: "bad".into(),
        };
        let err = client(config, &server).search("q", None).await.unwrap_err();
        assert!(err.to_string().contains("rejected the API key"), "{err}");
    }
}

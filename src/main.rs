use std::{collections::HashMap, fmt, string::FromUtf8Error};

use base64::{engine::general_purpose::URL_SAFE, Engine as _};
use chrono::{DateTime, SecondsFormat, Utc};
use fastly::{
    http::{header, HeaderName, HeaderValue, Method, StatusCode},
    kv_store::{KVStore, KVStoreError, LookupResponse},
    mime,
    secret_store::{LookupError, OpenError, SecretStore},
    Request, Response,
};
use rand::{seq::SliceRandom, thread_rng};
use random_word::Lang;
use serde::{de::value::Error as SerdeError, Deserialize, Serialize};
use serde_json::Error as JsonError;
use sha2::{Digest, Sha512};
use thiserror::Error;
use url::{ParseError, Url};

const URL_STORE: &str = "urls";
const MEMORABLE_WORD_STORE: &str = "memorable_words";
const URL_ADMIN_SECRET: &str = "url_admin";

#[derive(Error, Debug)]
enum Error {
    UrlParse(#[from] ParseError),
    Path(#[from] SerdeError),
    Json(#[from] JsonError),
    UrlNotFound(#[from] KVStoreError),
    DecodingUrl(#[from] FromUtf8Error),
    SecretStoreInitialization(#[from] OpenError),
    Authentication(#[from] LookupError),
    HeaderParsing(#[from] header::ToStrError),
    StoreNotFound(&'static str),
    UrlNotFoundDev,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UrlParse(e) => write!(f, "Error: {e}"),
            Self::Path(e) => write!(f, "Error: {e}"),
            Self::UrlNotFound(e) => write!(f, "Error: {e}"),
            Self::DecodingUrl(e) => write!(f, "Error: {e}"),
            Self::UrlNotFoundDev => write!(f, "Url Not Found, this will turn into an action later"),
            Self::SecretStoreInitialization(e) => write!(f, "Error: {e}"),
            Self::Json(e) => write!(f, "Error (de)serializing json metadata: {e}"),
            Self::Authentication(e) => write!(f, "Error: {e}"),
            Self::HeaderParsing(e) => write!(f, "Error: {e}"),
            Self::StoreNotFound(store) => write!(
                f,
                "The {store} store was not found, did you forget to setup?"
            ),
        }
    }
}
#[serde_with::serde_as]
#[derive(Deserialize, Serialize, Debug, Default)]
struct ItemMetadata {
    version: i32,
    time: String,
    #[serde_as(as = "HashMap<_, _>")]
    headers: HashMap<String, String>,
    hit_count: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    additional_data: Option<String>,
}

#[derive(Debug)]
struct Item {
    key: String,
    metadata: ItemMetadata,
}

fn verify_url(input: &str) -> Result<Url, Error> {
    Ok(Url::parse(input)?)
}

/// Checks the KVStore to see if a URL exists, if it does return
/// the full URL
///
/// Input to this function should be a validated, shortened URL string (taken through [`shorten`])
fn check_kv_store(input: &str) -> Result<LookupResponse, Error> {
    println!("Checking for shortened url {input}");
    //check the store exists and return an error if it does not
    let url_store = match KVStore::open(URL_STORE)? {
        Some(kv) => kv,
        None => return Err(Error::StoreNotFound(URL_STORE)),
    };

    match url_store.lookup(input) {
        Ok(res) => Ok(res),
        Err(KVStoreError::ItemNotFound) => Err(Error::UrlNotFoundDev)?,
        Err(e) => Err(e)?,
    }
}

/// Generates metadata from request and adds it to the store's stored metadata for each item
///
/// If a HeaderValue cannot be displayed as a string it will show up blank
fn generate_metadata(req: &Request) -> ItemMetadata {
    ItemMetadata {
        version: 0,
        headers: req
            .get_headers()
            .collect::<Vec<(&HeaderName, &HeaderValue)>>()
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or_default().to_owned()))
            .collect(),
        hit_count: 0,
        time: Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
        additional_data: None,
    }
}

/// Adds a shortened URL to the KV Store
///
/// This is meant to be after a [`check_kv_store`] only
fn add_to_kv_store(key: &str, value: &Url, req: &Request) -> Result<(), Error> {
    let metadata = generate_metadata(req);
    let url_store = match KVStore::open(URL_STORE)? {
        Some(kv) => kv,
        None => return Err(Error::StoreNotFound(URL_STORE)),
    };

    //use builder insert to add metadata for datetime
    url_store
        .build_insert()
        .metadata(serde_json::to_string(&metadata)?.as_str())
        .execute(key, value.as_str())?;
    Ok(())
}

// maybe if we add the scheme (https preferred?) and it's valid URL we can add it for them
//fn normalize_url(input: Url) -> Url {}

/// Shortens a URL to a 5 character unique hash that is comprised of
/// SHA512 on the raw URL, then base64 on the resulting hash
/// and finally truncate off the remaining base64 to use just the first
/// 5 characters
fn shorten(input: &Url) -> String {
    //SHA512 for uniqueness
    let mut hasher = Sha512::new();
    //Base64 URL encode for easier and better truncation
    //5 Characters for short URLs and typing
    hasher.update(input.as_str());
    let shad = hasher.finalize();
    let b_shad = URL_SAFE.encode(shad);
    b_shad[..5].to_string()
}

fn generate_word_length() -> usize {
    let word_lengths: [usize; 3] = [3, 4, 5];
    let mut rng = thread_rng();
    *word_lengths.choose(&mut rng).unwrap_or(&3)
}

fn shorten_with_words() -> String {
    // The odds of this failing are very low and there would be something else very wrong if it does fail
    let words: Vec<&str> = (0..3)
        .map(|_| random_word::gen_len(generate_word_length(), Lang::En).unwrap_or_default())
        .collect();
    words.join("-")
}

fn lookup_memorable_words(input: &Url) -> Result<Option<String>, Error> {
    let memorable_word_store = match KVStore::open(MEMORABLE_WORD_STORE)? {
        Some(kv) => kv,
        None => return Err(Error::StoreNotFound(MEMORABLE_WORD_STORE)),
    };

    let mut mem_word_response = memorable_word_store.lookup(input.as_ref())?;
    let string_response = mem_word_response.take_body().into_string();
    if string_response.is_empty() {
        Ok(None)
    } else {
        Ok(Some(string_response))
    }
}

/// Adds a shortened URL to the Memorable Word Store
fn add_to_memorable_word_store(key: &Url, value: &str, req: &Request) -> Result<(), Error> {
    let metadata = generate_metadata(req);
    let url_store = match KVStore::open(MEMORABLE_WORD_STORE)? {
        Some(kv) => kv,
        None => return Err(Error::StoreNotFound(MEMORABLE_WORD_STORE)),
    };

    url_store
        .build_insert()
        .metadata(serde_json::to_string(&metadata)?.as_str())
        .execute(key.as_str(), value)?;
    Ok(())
}

///Increases the hit count of the item in its metadata
fn increase_hit_count(key: &str, value: &str, response: &LookupResponse) -> Result<(), Error> {
    // If the item doesn't have metadata don't fail here
    let raw_metadata = response.metadata().unwrap_or_default();

    let string_metadata = String::from_utf8(raw_metadata.to_vec())?;
    let mut metadata: ItemMetadata = serde_json::from_str(&string_metadata)?;

    metadata.hit_count += 1;

    let url_store = match KVStore::open(URL_STORE)? {
        Some(kv) => kv,
        None => return Err(Error::StoreNotFound(URL_STORE)),
    };

    //use builder re-insert with new metadata
    url_store
        .build_insert()
        .metadata(serde_json::to_string(&metadata)?.as_str())
        .execute(key, value)?;

    Ok(())
}

/// Sorts all keys and then outputs the most recent of them
fn most_recent(keys: &mut [Item]) -> String {
    //sort by UTC from newest first (larger so reversed)
    keys.sort_unstable_by(|x, y| {
        DateTime::parse_from_rfc3339(&y.metadata.time)
            .unwrap_or_default()
            .cmp(&DateTime::parse_from_rfc3339(&x.metadata.time).unwrap_or_default())
    });

    let mut formatted_most_recent = String::from(
        r#"
    <div class="container-heading">
        Most Recent:
    </div></br></br>
    <div class="container-list">
    <ol>"#,
    );
    for value in keys.iter() {
        formatted_most_recent = format!("{}<li>{}</li>", formatted_most_recent, value.key);
    }

    format!("{}</ol></div>", formatted_most_recent)
}

/// Gets the top results of the most visited shortened urls
fn top_redirects(keys: &mut [Item]) -> String {
    //sort by number of hits, reverse so largest is at the top
    keys.sort_unstable_by(|x, y| y.metadata.hit_count.cmp(&x.metadata.hit_count));

    let mut formatted_most_visited = String::from(
        r#"
    <div class="container-heading">
        Most Visited:
    </div></br></br>
    <div class="container-list">
    <ol>"#,
    );
    for value in keys.iter() {
        formatted_most_visited = format!(
            "{}<li>({}) {}</li>",
            formatted_most_visited, value.metadata.hit_count, value.key
        );
    }

    format!("{}</ol></div>", formatted_most_visited)
}

#[fastly::main]
fn main(mut req: Request) -> Result<Response, Error> {
    // Log service version
    println!(
        "FASTLY_SERVICE_VERSION: {}",
        std::env::var("FASTLY_SERVICE_VERSION").unwrap_or_else(|_| String::new())
    );

    // Filter request methods...
    match req.get_method() {
        // Right now only allow GET requests
        &Method::GET | &Method::POST => (),
        // Block all requests with unexpected methods
        _ => {
            return Ok(Response::from_status(StatusCode::METHOD_NOT_ALLOWED)
                .with_header(header::ALLOW, "GET, POST")
                .with_body_text_plain("This method is not allowed\n"))
        }
    };

    // Pattern match on the path...
    match req.get_path() {
        "/" => {
            let html = include_str!("index.html");
            Ok(Response::from_status(StatusCode::OK)
                .with_content_type(mime::TEXT_HTML_UTF_8)
                .with_body(html))
        }
        "/style.css" => Ok(Response::from_status(StatusCode::OK)
            .with_content_type(mime::TEXT_CSS)
            .with_body(include_str!("style.css"))),
        "/favicon.ico" => Ok(Response::from_status(StatusCode::OK)
            .with_content_type(mime::IMAGE_PNG)
            .with_body(include_bytes!("favicon.ico").as_slice())),
        "/admin" => {
            let html = include_str!("admin.html");
            Ok(Response::from_status(StatusCode::OK)
                .with_content_type(mime::TEXT_HTML_UTF_8)
                .with_body(html))
        }
        "/admin-response" => {
            let url_admin_store = SecretStore::open(URL_ADMIN_SECRET)?;
            let params: HashMap<String, String> = req.take_body_form()?;
            let resp = url_admin_store.get(match params.get("username") {
                Some(username) => username,
                None => return Ok(Response::from_status(StatusCode::UNAUTHORIZED)),
            });
            match resp {
                Some(pwd) => match params.get("password") {
                    Some(pass) => {
                        if pwd.plaintext() == pass {
                            // put some cool stats here

                            //Get the KV store contents
                            let url_list = match KVStore::open(URL_STORE)? {
                                Some(kv) => kv,
                                None => return Err(Error::StoreNotFound(URL_STORE)),
                            };
                            let latest_keys_builder = url_list.build_list();
                            let mut latest_key_results = latest_keys_builder.execute()?;
                            let mut keys: Vec<String> = latest_key_results.keys().to_vec();

                            // iterate over the cursors to get all keys
                            while let Some(cursor) = latest_key_results.next_cursor() {
                                println!("finding next: {cursor}");
                                let next_keys_builder = url_list.build_list().cursor(&cursor);
                                latest_key_results = next_keys_builder.execute()?;
                                keys.extend(latest_key_results.keys().to_vec());
                            }

                            let mut metadata_keys: Vec<Item> = keys
                                .iter()
                                .map(|x| {
                                    let result = url_list.lookup(x).expect("Error finding key");
                                    let raw_metadata = result.metadata().unwrap_or_default();
                                    let string_metadata = String::from_utf8(raw_metadata.to_vec())
                                        .unwrap_or_default();
                                    let metadata: ItemMetadata =
                                        serde_json::from_str(&string_metadata).unwrap_or_default();
                                    Item {
                                        key: x.to_string(),
                                        metadata,
                                    }
                                })
                                .collect();

                            let most_recent_list = most_recent(&mut metadata_keys);
                            let most_visited_list = top_redirects(&mut metadata_keys);

                            // Fun information for the admin page!
                            let body = format!(
                                r#"
                                    <html>
                                        <head>
                                            <meta charset="UTF-8">
                                            <meta name="viewport" content="width=device-width, initial-scale=1.0">
                                            <title>LNKR</title>
                                            <link rel="stylesheet" href="style.css">
                                        </head>
                                        <body>
                                            <div class="container">
                                            <h1>Admin: Stats!</h1>
                                            <div class="white-container">
                                                {most_recent_list}
                                                </br></br>
                                                {most_visited_list}
                                            </div></div>
                                        </body>
                                    </html>"#
                            );
                            Ok(Response::from_status(StatusCode::OK)
                                .with_content_type(mime::TEXT_HTML_UTF_8)
                                .with_body(body))
                        } else {
                            Ok(Response::from_status(StatusCode::UNAUTHORIZED))
                        }
                    }
                    None => Ok(Response::from_status(StatusCode::UNAUTHORIZED)),
                },
                None => Ok(Response::from_status(StatusCode::UNAUTHORIZED)),
            }
        }
        "/process" => {
            let user_params: HashMap<String, String> = req.take_body_form()?;

            let custom_key = user_params.get("CKEY");
            let memorable_words = user_params.get("WORDS");
            let url = match user_params.get("URL") {
                Some(url) => url,
                None => return Ok(Response::from_status(StatusCode::BAD_REQUEST)),
            };
            //Strip the query params only if the option is set
            let verified_url = if user_params.contains_key("STRIP") {
                let mut url = verify_url(url)?;
                url.set_query(None);
                url
            } else {
                verify_url(url)?
            };

            // custom key always will exist with `Some("")` because html forms always send text keys
            // if both that and the memorable words is selected prefer the custom key
            // if not use the memorable words, other wise just encode the url as sha512 and base64
            let custom = custom_key.filter(|&x| !x.is_empty());

            let short_url = if let Some(c) = custom {
                c.replace(" ", "-").to_string() //do not allow spaces, TODO update this later for other non-URL safe characters
            } else if memorable_words.is_some() {
                // here since these words are random and not a custom key we should
                // first check if the URL has already been shorted by looking up the
                // verified URL and serving the result if there is one instead
                match lookup_memorable_words(&verified_url)? {
                    Some(shortened_url) => shortened_url,
                    None => {
                        // there was not a url here, so it's okay to shorten and add
                        let url_memorable = shorten_with_words();
                        add_to_memorable_word_store(&verified_url, &url_memorable, &req)?;
                        url_memorable
                    }
                }
            } else {
                shorten(&verified_url)
            };

            // check and see if the shortened url exists, if not add it
            // do this for memorable words as well since in the above check we
            // make sure those URLs are not added a bunch of times with random words
            let kv_result = match check_kv_store(&short_url) {
                Err(Error::UrlNotFoundDev) => {
                    let _ = add_to_kv_store(&short_url, &verified_url, &req);
                    Ok(verified_url.to_string())
                }
                Err(e) => Err(e),
                Ok(mut a) => Ok(a.take_body().into_string()),
            }?;

            let output = format!(
                r#"
    <html>

    <head>
        <meta charset="UTF-8">
        <meta name="viewport" content="width=device-width, initial-scale=1.0">
        <title>LNKR</title>
        <link rel="stylesheet" href="style.css">
    </head>

    <body>
        <div class="container">
            <h1>Your shortened URL!</h1>
            <div class="forum-form">
                <a href=https://lnkr.lol/{short_url}>https://lnkr.lol/{short_url}</a> => <a href={kv_result}>{kv_result}</a>
            </div>
        </div>
    </body>

    </html>
                    "#
            );

            Ok(Response::from_status(StatusCode::OK)
                .with_content_type(mime::TEXT_HTML_UTF_8)
                .with_body(output))
        }
        _ => {
            // For all other paths, check if there's a result in the KV Store
            // if there is _not_ a result throw a 404
            // if there is, redirect the user to the URL
            let key = &req.get_path().replace("/", "");
            match check_kv_store(key) {
                Ok(mut kv_response) => {
                    let url = kv_response.take_body().into_string();

                    // increase or start the count for this visited link
                    increase_hit_count(key, &url, &kv_response)?;

                    //now send them to the URL!
                    Ok(Response::temporary_redirect(url))
                }
                Err(e) => {
                    println!("Error there was no url or access error: {e}");
                    Ok(Response::from_status(StatusCode::NOT_FOUND)
                        .with_content_type(mime::TEXT_HTML_UTF_8)
                        .with_body(include_str!("404.html")))
                }
            }
        }
    }
}

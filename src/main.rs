use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use anyhow::{bail, Context};
use clap::Parser;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use reqwest::Client;
use scraper::{Html, Selector};
use serde::Deserialize;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tokio::time::{sleep, Duration};

// Program defaults
const DEFAULT_OUTPUT: &str = "downloads";
const DEFAULT_PARALLEL_ARTICLES: usize = 4;
const DEFAULT_PARALLEL_IMAGES: usize = 3;
const DEFAULT_DELAY: u64 = 250;
const HTTP_TIMEOUT_SECS: u64 = 30;

const BASE_URL: &str = "https://basicappleguy.com";
const CATEGORY_PATH: &str = "/basicappleblog/category/Wallpaper";
const PRE_DOWNLOAD_DELAY_MS: u64 = 100;
const MAX_RETRIES: u32 = 3;
const RETRY_BASE_DELAY_MS: u64 = 1000;

/// Download wallpapers from Basic Apple Guy
#[derive(Parser)]
#[command(name = "wallpaper-downloader", version)]
struct Args {
    /// Output directory for downloaded wallpapers
    #[arg(short, long)]
    output: Option<PathBuf>,

    /// Maximum number of articles to fetch concurrently
    #[arg(short = 'j', long, value_parser = clap::value_parser!(u64).range(1..))]
    parallel_articles: Option<u64>,

    /// Maximum number of images to download concurrently per article
    #[arg(short = 'i', long, value_parser = clap::value_parser!(u64).range(1..))]
    parallel_images: Option<u64>,

    /// Sort images into device subfolders (iPhone/, iPad/, Mac/, Others/)
    #[arg(long)]
    device_sort: Option<bool>,

    /// Sort images into article subfolders
    #[arg(long)]
    article_sort: Option<bool>,

    /// Delay in milliseconds between downloads (respectful scraping)
    #[arg(long)]
    delay: Option<u64>,

    /// Open the output folder when finished
    #[arg(long)]
    open: bool,

    /// Path to config file
    #[arg(short, long)]
    config: Option<PathBuf>,
}

/// TOML config file structure. All fields are optional.
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct Config {
    output: Option<PathBuf>,
    parallel_articles: Option<usize>,
    parallel_images: Option<usize>,
    device_sort: Option<bool>,
    article_sort: Option<bool>,
    delay: Option<u64>,
    open_on_finish: Option<bool>,
}

/// Resolved settings after merging CLI args > config file > program defaults.
#[derive(Clone)]
struct Settings {
    output: PathBuf,
    parallel_articles: usize,
    parallel_images: usize,
    device_sort: bool,
    article_sort: bool,
    delay: u64,
    open_on_finish: bool,
}

/// Counts returned from processing an article.
struct ArticleResult {
    downloaded: u32,
    skipped: u32,
}

fn expand_path(path: &Path) -> PathBuf {
    match path.to_str() {
        Some(s) => PathBuf::from(shellexpand::full(s).unwrap_or(s.into()).into_owned()),
        None => path.to_path_buf(),
    }
}

fn load_config(path: &Path) -> anyhow::Result<Config> {
    let contents = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read config file {}", path.display()))?;
    toml::from_str(&contents)
        .with_context(|| format!("failed to parse config file {}", path.display()))
}

fn merge_settings(args: Args, config: Config) -> anyhow::Result<Settings> {
    let output = args
        .output
        .or(config.output)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_OUTPUT));
    let output = expand_path(&output);

    let parallel_articles = args
        .parallel_articles
        .map(|v| v as usize)
        .or(config.parallel_articles)
        .unwrap_or(DEFAULT_PARALLEL_ARTICLES);
    let parallel_images = args
        .parallel_images
        .map(|v| v as usize)
        .or(config.parallel_images)
        .unwrap_or(DEFAULT_PARALLEL_IMAGES);

    if parallel_articles == 0 {
        bail!("parallel_articles must be at least 1");
    }
    if parallel_images == 0 {
        bail!("parallel_images must be at least 1");
    }

    Ok(Settings {
        output,
        parallel_articles,
        parallel_images,
        device_sort: args
            .device_sort
            .or(config.device_sort)
            .unwrap_or(true),
        article_sort: args
            .article_sort
            .or(config.article_sort)
            .unwrap_or(true),
        delay: args.delay.or(config.delay).unwrap_or(DEFAULT_DELAY),
        open_on_finish: args.open || config.open_on_finish.unwrap_or(false),
    })
}

fn resolve_settings(args: Args) -> anyhow::Result<Settings> {
    let config = if let Some(ref path) = args.config {
        load_config(path)?
    } else {
        let local = Path::new("config.toml");
        if local.exists() {
            load_config(local)?
        } else if let Some(global) = dirs::config_dir()
            .map(|d| d.join("wallpaper-downloader").join("config.toml"))
            .filter(|p| p.exists())
        {
            load_config(&global)?
        } else {
            Config::default()
        }
    };

    merge_settings(args, config)
}

fn categorize_filename(filename: &str) -> &str {
    let lower = filename.to_lowercase();
    if lower.contains("iphone") {
        "iPhone"
    } else if lower.contains("ipad") {
        "iPad"
    } else if lower.contains("mac") {
        "Mac"
    } else {
        "Others"
    }
}

/// Extract a short article name from a URL like "https://basicappleguy.com/basicappleblog/some-wallpaper"
fn article_name(url: &str) -> &str {
    url.rsplit('/').next().unwrap_or(url)
}

fn build_client() -> anyhow::Result<Client> {
    Client::builder()
        .user_agent("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.0 Safari/605.1.15")
        .timeout(Duration::from_secs(HTTP_TIMEOUT_SECS))
        .build()
        .context("failed to build HTTP client")
}

async fn download_image(client: &Client, url: &str, path: &Path) -> anyhow::Result<()> {
    let response = client
        .get(url)
        .send()
        .await
        .context("failed to send request")?
        .error_for_status()
        .with_context(|| format!("server returned error for {url}"))?;

    let bytes = response
        .bytes()
        .await
        .with_context(|| format!("failed to read response body from {url}"))?;

    tokio::fs::write(path, &bytes)
        .await
        .with_context(|| format!("failed to write {}", path.display()))?;

    Ok(())
}

async fn download_with_retry(client: &Client, url: &str, path: &Path) -> anyhow::Result<()> {
    let mut last_err = anyhow::anyhow!("no attempts made");
    for attempt in 0..=MAX_RETRIES {
        if attempt > 0 {
            let backoff = RETRY_BASE_DELAY_MS * (1 << (attempt - 1));
            sleep(Duration::from_millis(backoff)).await;
        }
        match download_image(client, url, path).await {
            Ok(()) => return Ok(()),
            Err(e) => last_err = e,
        }
    }
    Err(last_err)
}

/// Fetch a category page, returning the HTML body.
/// On failure, returns None if we already have articles (tolerant pagination),
/// or propagates the error if this is the first page.
async fn fetch_page(
    client: &Client,
    page_url: &str,
    have_articles: bool,
    spinner: &ProgressBar,
) -> anyhow::Result<Option<String>> {
    let response = match client.get(page_url).send().await {
        Ok(r) => match r.error_for_status() {
            Ok(r) => r,
            Err(e) => {
                if !have_articles {
                    return Err(e).context("failed to fetch category page");
                }
                spinner.println(format!(
                    "Warning: pagination failed at {page_url}: {e}, continuing with articles found so far"
                ));
                return Ok(None);
            }
        },
        Err(e) => {
            if !have_articles {
                return Err(e).context("failed to fetch category page");
            }
            spinner.println(format!(
                "Warning: pagination failed at {page_url}: {e}, continuing with articles found so far"
            ));
            return Ok(None);
        }
    };

    match response.text().await {
        Ok(html) => Ok(Some(html)),
        Err(e) => {
            if !have_articles {
                return Err(e).context("failed to read category page body");
            }
            spinner.println(format!(
                "Warning: failed to read page body at {page_url}: {e}, continuing with articles found so far"
            ));
            Ok(None)
        }
    }
}

/// Discover all wallpaper article URLs by paginating through the category listing.
/// Tolerates failures on non-first pages — returns whatever was found so far.
async fn discover_articles(
    client: &Client,
    spinner: &ProgressBar,
) -> anyhow::Result<Vec<String>> {
    let mut article_urls: Vec<String> = Vec::new();
    let mut seen = HashSet::new();
    let mut next_url = Some(format!("{BASE_URL}{CATEGORY_PATH}"));
    let article_selector = Selector::parse(".blog-title a").expect("valid selector");
    let next_selector = Selector::parse("a[rel=\"next\"]").expect("valid selector");

    while let Some(page_url) = next_url.take() {
        spinner.set_message(format!(
            "Searching for articles... (found {} so far)",
            article_urls.len()
        ));

        let html = match fetch_page(client, &page_url, !article_urls.is_empty(), spinner).await? {
            Some(html) => html,
            None => break,
        };

        let document = Html::parse_document(&html);

        let page_articles: Vec<String> = document
            .select(&article_selector)
            .filter_map(|el| el.value().attr("href"))
            .map(|href| format!("{BASE_URL}{href}"))
            .filter(|url| seen.insert(url.clone()))
            .collect();

        if page_articles.is_empty() {
            break;
        }

        article_urls.extend(page_articles);

        if let Some(next_href) = document
            .select(&next_selector)
            .next()
            .and_then(|el| el.value().attr("href"))
        {
            next_url = Some(format!("{BASE_URL}{next_href}"));
            sleep(Duration::from_millis(PRE_DOWNLOAD_DELAY_MS)).await;
        }
    }

    Ok(article_urls)
}

/// Process a single article: fetch its page, find image links, and download them.
async fn process_article(
    client: &Client,
    page_url: &str,
    settings: &Settings,
    image_selector: &Selector,
    pb: ProgressBar,
) -> anyhow::Result<ArticleResult> {
    let name = article_name(page_url);
    pb.set_message(format!("{name}: fetching page..."));

    let response = client
        .get(page_url)
        .send()
        .await
        .with_context(|| format!("failed to fetch article {page_url}"))?
        .error_for_status()
        .with_context(|| format!("server returned error for article {page_url}"))?;

    let html = response
        .text()
        .await
        .with_context(|| format!("failed to read article body {page_url}"))?;

    let hrefs: Vec<String> = {
        let document = Html::parse_document(&html);
        let mut seen = HashSet::new();
        document
            .select(image_selector)
            .filter_map(|el| {
                el.value()
                    .attr("href")
                    .filter(|h| h.starts_with("/s/"))
                    .map(String::from)
            })
            .filter(|href| seen.insert(href.clone()))
            .collect()
    };

    let total = hrefs.len();
    pb.set_length(total as u64);
    pb.set_message(format!("{name}: scanning {total} images..."));

    // Prepare download tasks (resolve destinations up front so dir creation is sequential)
    let mut image_tasks: Vec<(String, PathBuf, String)> = Vec::new();
    let mut already_existed: u32 = 0;
    for href in &hrefs {
        let filename = href.strip_prefix("/s").unwrap();
        let filename = filename.trim_start_matches('/');

        if filename.is_empty() {
            pb.inc(1);
            continue;
        }

        let mut dest_dir = settings.output.clone();
        if settings.device_sort {
            dest_dir.push(categorize_filename(filename));
        }
        if settings.article_sort {
            dest_dir.push(name);
        }
        tokio::fs::create_dir_all(&dest_dir).await.with_context(|| {
            format!("failed to create directory {}", dest_dir.display())
        })?;
        let dest = dest_dir.join(filename);

        if tokio::fs::try_exists(&dest).await.unwrap_or(false) {
            already_existed += 1;
            pb.inc(1);
            continue;
        }

        let download_url = format!("{BASE_URL}{href}");
        image_tasks.push((download_url, dest, filename.to_string()));
    }

    if already_existed > 0 {
        let to_download = image_tasks.len();
        pb.set_message(format!("{name}: {already_existed} cached, downloading {to_download}..."));
    }

    let downloaded = Arc::new(AtomicU32::new(0));
    let img_semaphore = Arc::new(Semaphore::new(settings.parallel_images));
    let mut img_set = JoinSet::new();

    for (download_url, dest, filename) in image_tasks {
        let permit = img_semaphore
            .clone()
            .acquire_owned()
            .await
            .context("image semaphore closed")?;
        let client = client.clone();
        let pb = pb.clone();
        let downloaded = downloaded.clone();
        let name = name.to_string();
        let delay_ms = settings.delay;

        img_set.spawn(async move {
            sleep(Duration::from_millis(PRE_DOWNLOAD_DELAY_MS)).await;

            match download_with_retry(&client, &download_url, &dest).await {
                Ok(()) => {
                    let count = downloaded.fetch_add(1, Ordering::Relaxed) + 1;
                    pb.inc(1);
                    pb.set_message(format!("{name}: {count}/{total} images"));
                }
                Err(e) => {
                    pb.inc(1);
                    pb.println(format!("  Error: failed to download {filename}: {e:#}"));
                }
            }

            sleep(Duration::from_millis(delay_ms)).await;
            drop(permit);
        });
    }

    while let Some(result) = img_set.join_next().await {
        if let Err(e) = result {
            pb.println(format!("  Warning: image task panicked: {e}"));
        }
    }

    let final_count = downloaded.load(Ordering::Relaxed);
    let msg = if already_existed > 0 {
        format!("  {name}: done ({final_count} new, {already_existed} skipped)")
    } else {
        format!("  {name}: done ({final_count} images)")
    };
    pb.println(msg);
    pb.finish_and_clear();
    Ok(ArticleResult {
        downloaded: final_count,
        skipped: already_existed,
    })
}

/// Spawn article processing tasks and collect results.
async fn download_articles(
    client: &Client,
    article_urls: Vec<String>,
    settings: &Settings,
    mp: &MultiProgress,
) -> anyhow::Result<(u32, u32)> {
    let article_count = article_urls.len() as u64;

    let overall_style = ProgressStyle::with_template(
        "{spinner:.green} [{bar:30.green/dim}] {pos}/{len} articles {msg}",
    )
    .unwrap()
    .progress_chars("━╸─")
    .tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏");

    let overall = mp.add(ProgressBar::new(article_count));
    overall.set_style(overall_style);
    overall.enable_steady_tick(Duration::from_millis(100));

    let article_style = ProgressStyle::with_template(
        "  {spinner:.blue} [{bar:20.blue/dim}] {pos}/{len} {msg}",
    )
    .unwrap()
    .progress_chars("━╸─")
    .tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏");

    let semaphore = Arc::new(Semaphore::new(settings.parallel_articles));
    let mut tasks = JoinSet::new();

    let task_settings = Arc::new(settings.clone());
    // Parse once, share across all article tasks
    let image_selector = Arc::new(Selector::parse("p a").expect("valid selector"));

    for page_url in article_urls {
        let permit = semaphore
            .clone()
            .acquire_owned()
            .await
            .context("semaphore closed")?;
        let client = client.clone();
        let task_settings = task_settings.clone();
        let image_selector = image_selector.clone();

        let pb = mp.insert_before(&overall, ProgressBar::new(0));
        pb.set_style(article_style.clone());
        pb.enable_steady_tick(Duration::from_millis(100));

        let overall = overall.clone();
        tasks.spawn(async move {
            let result =
                process_article(&client, &page_url, &task_settings, &image_selector, pb).await;
            overall.inc(1);
            drop(permit);
            result
        });
    }

    let mut total_downloaded = 0u32;
    let mut total_skipped = 0u32;

    while let Some(result) = tasks.join_next().await {
        match result {
            Ok(Ok(r)) => {
                total_downloaded += r.downloaded;
                total_skipped += r.skipped;
            }
            Ok(Err(e)) => overall.println(format!("Warning: article failed: {e:#}")),
            Err(e) => overall.println(format!("Warning: task panicked: {e}")),
        }
    }

    overall.finish_and_clear();
    Ok((total_downloaded, total_skipped))
}

fn open_folder(path: &Path) {
    let cmd = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(target_os = "windows") {
        "explorer"
    } else {
        "xdg-open"
    };
    if let Err(e) = std::process::Command::new(cmd).arg(path).spawn() {
        eprintln!("Warning: failed to open folder: {e}");
    }
}

async fn run() -> anyhow::Result<()> {
    let args = Args::parse();
    let settings = resolve_settings(args)?;

    let mp = MultiProgress::new();

    let spinner_style = ProgressStyle::with_template("{spinner:.cyan} {msg}")
        .unwrap()
        .tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏");

    let spinner = mp.add(ProgressBar::new_spinner());
    spinner.set_style(spinner_style);
    spinner.enable_steady_tick(Duration::from_millis(100));
    spinner.set_message("Searching for images...");

    tokio::fs::create_dir_all(&settings.output)
        .await
        .with_context(|| format!("failed to create output directory {}", settings.output.display()))?;

    let client = build_client()?;

    let article_urls = discover_articles(&client, &spinner).await?;
    spinner.finish_and_clear();

    let (total_downloaded, total_skipped) =
        download_articles(&client, article_urls, &settings, &mp).await?;
    mp.clear().ok();

    if total_skipped > 0 {
        println!(
            "Done. {total_downloaded} new, {total_skipped} already existed in {}",
            settings.output.display()
        );
    } else {
        println!(
            "Done. Downloaded {total_downloaded} images to {}",
            settings.output.display()
        );
    }

    if settings.open_on_finish {
        open_folder(&settings.output);
    }

    Ok(())
}

#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        eprintln!("Error: {e:#}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn categorize_priority() {
        let cases = [
            ("iphone-ipad-mac-wallpaper.png", "iPhone"),
            ("ipad-mac-wallpaper.png", "iPad"),
            ("mac-wallpaper.png", "Mac"),
            ("wallpaper.png", "Others"),
        ];
        for (input, expected) in cases {
            assert_eq!(
                categorize_filename(input),
                expected,
                "categorize_filename({input:?}) should be {expected:?}"
            );
        }
    }

    #[test]
    fn config_rejects_unknown_fields() {
        assert!(toml::from_str::<Config>(r#"parellel_articles = 4"#).is_err());
    }

    fn default_args() -> Args {
        Args {
            output: None,
            parallel_articles: None,
            parallel_images: None,
            device_sort: None,
            article_sort: None,
            delay: None,
            open: false,
            config: None,
        }
    }

    #[test]
    fn merge_config_overrides_defaults() {
        let config = Config {
            output: Some(PathBuf::from("from_config")),
            parallel_articles: Some(10),
            parallel_images: Some(6),
            device_sort: Some(false),
            article_sort: Some(false),
            delay: Some(999),
            open_on_finish: Some(true),
        };
        let s = merge_settings(default_args(), config).unwrap();

        assert_eq!(s.output, PathBuf::from("from_config"));
        assert_eq!(s.parallel_articles, 10);
        assert_eq!(s.parallel_images, 6);
        assert_eq!(s.device_sort, false);
        assert_eq!(s.article_sort, false);
        assert_eq!(s.delay, 999);
        assert_eq!(s.open_on_finish, true);
    }

    #[test]
    fn merge_cli_overrides_config() {
        let args = Args {
            output: Some(PathBuf::from("cli_output")),
            parallel_articles: Some(2),
            parallel_images: Some(1),
            device_sort: Some(true),
            article_sort: Some(true),
            delay: Some(100),
            open: false,
            config: None,
        };
        let config = Config {
            output: Some(PathBuf::from("config_output")),
            parallel_articles: Some(10),
            parallel_images: Some(6),
            device_sort: Some(false),
            article_sort: Some(false),
            delay: Some(999),
            open_on_finish: Some(true),
        };
        let s = merge_settings(args, config).unwrap();

        assert_eq!(s.output, PathBuf::from("cli_output"));
        assert_eq!(s.parallel_articles, 2);
        assert_eq!(s.parallel_images, 1);
        assert_eq!(s.device_sort, true);
        assert_eq!(s.article_sort, true);
        assert_eq!(s.delay, 100);
        assert_eq!(s.open_on_finish, true);
    }

    #[test]
    fn merge_falls_back_to_defaults() {
        let s = merge_settings(default_args(), Config::default()).unwrap();

        assert_eq!(s.output, PathBuf::from(DEFAULT_OUTPUT));
        assert_eq!(s.parallel_articles, DEFAULT_PARALLEL_ARTICLES);
        assert_eq!(s.parallel_images, DEFAULT_PARALLEL_IMAGES);
        assert_eq!(s.device_sort, true);
        assert_eq!(s.article_sort, true);
        assert_eq!(s.delay, DEFAULT_DELAY);
        assert_eq!(s.open_on_finish, false);
    }

    #[test]
    fn merge_rejects_zero_parallel_articles() {
        let config = Config {
            parallel_articles: Some(0),
            ..Config::default()
        };
        assert!(merge_settings(default_args(), config).is_err());
    }

    #[test]
    fn merge_rejects_zero_parallel_images() {
        let config = Config {
            parallel_images: Some(0),
            ..Config::default()
        };
        assert!(merge_settings(default_args(), config).is_err());
    }
}

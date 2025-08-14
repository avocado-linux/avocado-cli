use std::{
    cmp::min,
    env,
    fs::{create_dir_all, rename},
    io::{Cursor, ErrorKind, Seek, Write},
    path::Path,
};

use clap::Parser;
use directories::ProjectDirs;
use flate2::read::GzDecoder;
use futures_util::StreamExt;
use indicatif::{ProgressBar, ProgressStyle};
use reqwest::ClientBuilder;
use serde::Deserialize;
use tar::Archive;

#[derive(Deserialize, Debug)]
struct GithubAssetResponse {
    browser_download_url: String,
    name: String,
}

#[derive(Deserialize, Debug)]
struct GithubResponse {
    tag_name: String,
    assets: Vec<GithubAssetResponse>,
}

#[derive(Parser, Debug)]
pub struct UpgradeCommand {
    /// Controls what version to upgrade to.
    ///
    /// If not specified, the latest version will be used.
    #[arg(long)]
    pub version: Option<String>,
}

impl UpgradeCommand {
    pub async fn run(self) -> anyhow::Result<()> {
        if let Some(proj_dirs) = ProjectDirs::from("", "", "avocado") {
            let cache_dir = proj_dirs.cache_dir();

            create_dir_all(cache_dir).unwrap();

            let requested_version = self.version.clone();
            if let Ok(resp) = Self::get_release_info(self.version).await {
                let current_version = env!("CARGO_PKG_VERSION");

                // no need to update
                if resp.tag_name == current_version {
                    println!("CLI already up to date");
                    return Ok(());
                }

                let name = format!("avocado-{}_{}.tar.gz", resp.tag_name, env!("TARGET"));

                let github_asset_info = match resp.assets.iter().find(|&x| x.name == name) {
                    Some(x) => x,
                    None => {
                        println!(
                            "version {} does not include a pre-built binary for target {}",
                            resp.tag_name,
                            env!("TARGET")
                        );
                        return Ok(());
                    }
                };

                if let Err(message) = Self::download_update(cache_dir, github_asset_info).await {
                    println!("{message}");
                    return Ok(());
                }

                if let Err(message) = Self::apply_update(cache_dir, &resp) {
                    println!("{message}");
                    return Ok(());
                }
            } else if let Some(v) = requested_version {
                // If the requested version doesn't exist, surface a friendly message
                println!("No releases match version '{v}'");
                return Ok(());
            }
        }

        Ok(())
    }

    fn apply_update(path: &Path, github_response: &GithubResponse) -> Result<(), String> {
        let update_file = path.join("avocado");

        let current_cli_executable =
            env::current_exe().map_err(|_| "Can't retrieve the current cli directory")?;

        if let Err(err) = rename(update_file, &current_cli_executable) {
            if err.kind() == ErrorKind::PermissionDenied {
                return Err(format!(
                    "CLI failed to upgrade: permission denied writing to {}",
                    &current_cli_executable.display()
                ));
            }
            return Err(format!(
                "CLI failed to upgrade: unknown error writing to {}",
                &current_cli_executable.display()
            ));
        }

        println!("CLI upgraded successfully ({})", github_response.tag_name);

        Ok(())
    }

    async fn download_update(
        download_path: &Path,
        github_asset_info: &GithubAssetResponse,
    ) -> Result<(), String> {
        let client = ClientBuilder::new().use_rustls_tls().build().unwrap();
        let url = &github_asset_info.browser_download_url;

        // Reqwest setup
        let res = client
            .get(url)
            .send()
            .await
            .map_err(|_| format!("Failed to GET from '{url}'"))?;

        let total_size = res
            .content_length()
            .ok_or(format!("Failed to get content length from '{url}'"))?;

        // Indicatif setup
        let pb = ProgressBar::new(total_size);
        pb.set_style(ProgressStyle::with_template("{msg}\n{spinner:.green} [{elapsed_precise}] [{wide_bar:.cyan/blue}] {bytes}/{total_bytes} ({bytes_per_sec}, {eta})")
			.unwrap()
			.progress_chars("#>-"));

        // download chunks
        let mut downloaded: u64 = 0;

        let mut stream = res.bytes_stream();

        let mem = Vec::new();
        let mut buff = Cursor::new(mem);

        while let Some(item) = stream.next().await {
            let chunk = item.map_err(|_| "Error while downloading file".to_string())?;
            buff.write_all(&chunk)
                .map_err(|_| "Error while writing to buffer".to_string())?;
            let new = min(downloaded + (chunk.len() as u64), total_size);
            downloaded = new;
            pb.set_position(new);
        }

        pb.finish_and_clear();

        buff.rewind().unwrap();

        let gz = GzDecoder::new(&mut buff);

        let mut archive = Archive::new(gz);

        archive
            .unpack(download_path)
            .map_err(|_| "Error while saving the updated file".to_string())?;

        Ok(())
    }

    async fn get_release_info(version: Option<String>) -> Result<GithubResponse, reqwest::Error> {
        let client = ClientBuilder::new().use_rustls_tls().build()?;
        let url = if let Some(version) = version {
            format!(
                "https://api.github.com/repos/avocado-linux/avocado-cli/releases/tags/{version}"
            )
        } else {
            "https://api.github.com/repos/avocado-linux/avocado-cli/releases/latest".to_owned()
        };

        let resp = client
            .get(url)
            .header("Accept", "application/vnd.github+json")
            .header("User-Agent", "avocado-linux/avocado-cli")
            .send()
            .await?;

        let resp = resp.error_for_status()?;
        resp.json::<GithubResponse>().await
    }
}

// Utility for downloading raw FIT files from a Garmin Connect account.
//
// Heavily indebted to tapiriik; implementation below closely follows
// https://github.com/cpfair/tapiriik/blob/master/tapiriik/services/GarminConnect/garminconnect.py

use actix_web::{client, cookie, http};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

use std::collections::HashSet;
use std::time::Duration;

use async_std::task;
use std::path::Path;

use regex::Regex;

use anyhow::{anyhow, Context, Result};
use std::fs::{File, OpenOptions};
use std::io::prelude::*;
use std::io::BufReader;

use clap::{App, Arg};

#[derive(Serialize, Deserialize)]
struct MyCookie {
    orig: Vec<u8>,
}

impl MyCookie {
    fn new(orig: Vec<u8>) -> Self {
        MyCookie { orig }
    }

    fn as_cookie(&self) -> Result<cookie::Cookie> {
        let as_str = String::from_utf8(self.orig.clone())?;
        let new_cookie = actix_web::cookie::Cookie::parse(as_str)
            .with_context(|| "error constructing cookie".to_string())?;
        Ok(new_cookie)
    }
}

#[derive(Serialize, Deserialize)]
struct MyCookieJar {
    cookies: HashMap<String, MyCookie>, // cookie_name, cookie_bytes
    file_path: String,
}

impl MyCookieJar {
    fn new(path: &'static str) -> Self {
        MyCookieJar {
            cookies: HashMap::new(),
            file_path: path.to_string(),
        }
    }

    fn add(&mut self, cookie_bytes: Vec<u8>) -> Result<()> {
        let as_str = String::from_utf8(cookie_bytes.clone())?;
        let as_cookie = actix_web::cookie::Cookie::parse(as_str)
            .with_context(|| "error constructing cookie".to_string())?;

        self.cookies
            .insert(as_cookie.name().to_string(), MyCookie::new(cookie_bytes));
        Ok(())
    }

    fn save(&self) -> Result<()> {
        let file = OpenOptions::new()
            .write(true)
            .truncate(true)
            .create(true)
            .open(self.file_path.clone())?;

        serde_json::to_writer(&file, self)?;
        Ok(())
    }

    fn has_saved_cookies(&self) -> bool {
        Path::new(&self.file_path).exists()
    }

    fn load(&self) -> Result<Self> {
        let file = File::open(self.file_path.clone())?;
        let reader = BufReader::new(file);

        let jar = serde_json::from_reader(reader)?;
        Ok(jar)
    }

    fn get_cookies(&self) -> Result<Vec<cookie::Cookie>> {
        let mut res: Vec<cookie::Cookie> = vec![];
        for mycookie in self.cookies.values() {
            res.push(mycookie.as_cookie()?);
        }
        Ok(res)
    }
}

/*
pub trait ClientRequestExt {
    fn add_cookies(self, cookies: &MyCookieJar) -> Self;
}

impl ClientRequestExt for client::ClientRequest {
    fn add_cookies(mut self, cookies: &MyCookieJar) -> Self {
        for cookie in cookies.get_cookies().unwrap() {
            self.cookie(cookie);
        }

        self
    }
}
*/

#[derive(Serialize, Deserialize)]
struct ActivityDownloadIndex(HashMap<u64, String>); // activityId, path_to_activity_file

impl ActivityDownloadIndex {
    fn new() -> Self {
        ActivityDownloadIndex(HashMap::new())
    }
}

#[derive(Serialize, Deserialize)]
struct ActivityDownloads {
    index_file_path: String,
    activity_download_directory: String,
    index: ActivityDownloadIndex,
}

impl ActivityDownloads {
    fn new(index_file_path: String, activity_download_directory: String) -> Result<Self> {
        let a_idx = match Path::new(&index_file_path).exists() {
            true => {
                let file = File::open(&index_file_path)?;
                let reader = BufReader::new(file);
                serde_json::from_reader(reader)?
            }
            false => ActivityDownloadIndex::new(),
        };

        let dl_path = Path::new(&activity_download_directory);

        if !dl_path.exists() {
            std::fs::create_dir(dl_path)?
        }

        Ok(ActivityDownloads {
            index_file_path,
            activity_download_directory,
            index: a_idx,
        })
    }

    fn update(&mut self, new_download: (u64, String)) -> Result<()> {
        let (activity_id, activity_path) = new_download;

        self.index.0.insert(activity_id, activity_path);

        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .open(self.index_file_path.clone())?;

        serde_json::to_writer(&file, &self.index)?;
        Ok(())
    }

    fn not_yet_downloaded(&self, all_activities: HashSet<u64>) -> HashSet<u64> {
        let already_downloaded: HashSet<u64> = self.index.0.keys().cloned().collect();
        all_activities
            .difference(&already_downloaded)
            .cloned()
            .collect()
    }
}

const PREVIOUSLY_DOWNLOADED_ACTIVITIES_INDEX: &str = "./previously_downloaded_activities";
const ACTIVITY_DOWNLOAD_DIRECTORY: &str = "./downloaded_activities";
const LOGIN_COOKIES_FILE: &str = "./garmin_connect_cookies";
const USER_AGENT: &str =
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10.15; rv:78.0) Gecko/20100101 Firefox/78.0)";

enum RequestType {
    GET,
    POST,
}

struct GarminConnect {
    jar: MyCookieJar,
    username: String,
    password: String,
    activity_downloads: ActivityDownloads,
    client: client::Client,
}

impl GarminConnect {
    async fn new(
        username: String,
        password: String,
        activity_download_directory: String,
        previously_downloaded_activities_index: String,
    ) -> Result<Self> {
        let jar = MyCookieJar::new(LOGIN_COOKIES_FILE);
        let adls = ActivityDownloads::new(
            previously_downloaded_activities_index,
            activity_download_directory,
        )?;

        let mut gc = GarminConnect {
            jar,
            username,
            password,
            activity_downloads: adls,
            client: client::Client::new(),
        };

        if gc.jar.has_saved_cookies() {
            gc.jar = gc.jar.load()?;
            if let Err(e) = gc.test_auth().await {
                println!(
                    "error testing auth with saved cookies: {:?}; logging in again",
                    e
                );
                gc.login().await?;
            }
        } else {
            gc.login().await?;
        }

        Ok(gc)
    }

    fn make_request(&self, request_type: RequestType, url: String) -> client::ClientRequest {
        let mut req = match request_type {
            RequestType::GET => self.client.get(url),
            RequestType::POST => self.client.post(url),
        };

        req = req.header("USER-AGENT", USER_AGENT);

        for cookie in self.jar.get_cookies().unwrap() {
            req = req.cookie(cookie);
        }

        req
    }

    fn save_cookies(&mut self, headers: &http::header::HeaderMap) -> Result<()> {
        println!("save_cookies");
        for cookie_str in headers.get_all("set-cookie") {
            let bytes = cookie_str.as_bytes().to_vec();
            self.jar.add(bytes.clone())?;
        }
        Ok(())
    }

    async fn test_auth(&self) -> Result<()> {
        task::sleep(Duration::from_secs(1)).await;
        println!("testing authorization");

        let mut response = self
            .make_request(
                RequestType::GET,
                "https://connect.garmin.com/modern/".to_string(),
            )
            .send()
            .await
            .map_err(|e| anyhow::Error::msg(format!("{:?}", e)))?;

        if response.status() != 200 {
            let url = String::from_utf8(
                response
                    .headers()
                    .get("location")
                    .unwrap()
                    .as_bytes()
                    .to_vec(),
            )?;
            println!(
                "test_auth status = {}, location = {}",
                response.status(),
                url
            );
            return Err(anyhow!("test_auth failed, status = {}", response.status()));
        }

        let body = response
            .body()
            .await
            .map_err(|e| anyhow::Error::msg(format!("{:?}", e)))?;

        // verify that the login was successful
        let mut body_str = String::from_utf8(body.to_vec())?;
        body_str = body_str.replace("\n", "");

        let re = Regex::new(r##"VIEWER_USERPREFERENCES = JSON.parse\("(?P<json>.+?)"\)"##)?;
        let captures = re.captures(&body_str).unwrap();
        let json = captures.name("json").unwrap().as_str();
        let raw = json.replace("\\", "");

        let v: Value = serde_json::from_str(&raw)?;
        match &v["displayName"] {
            serde_json::Value::String(u) => {
                if u == &self.username {
                    Ok(())
                } else {
                    Err(anyhow!("username from login doesn't match supplied username"))
                }
            }
            _ => Err(anyhow!("couldn't find username in login response"))
        }
    }

    async fn login(&mut self) -> Result<()> {
        //let mut data: HashMap<&str, &str> = HashMap::new();
        //data.insert("username", &self.username);
        //data.insert("password", &self.password);

        let mut params: HashMap<&str, &str> = HashMap::new();
        params.insert("service", "https://connect.garmin.com/modern");
        params.insert("clientId", "GarminConnect");
        params.insert("gauthHost", "https://sso.garmin.com/sso");
        params.insert("consumeServiceTicket", "false");

        /*
        let mut post_params: HashMap<&str, &str> = HashMap::new();
        post_params.insert("service", "https://connect.garmin.com/modern");
        post_params.insert("webhost", "https://connect.garmin.com/modern");
        post_params.insert("source", "https://connect.garmin.com/signin/");
        post_params.insert("redirectAfterAccountLoginUrl", "https://connect.garmin.com/modern/");
        post_params.insert("redirectAfterAccountCreationUrl", "https://connect.garmin.com/modern/");
        post_params.insert("gauthHost", "https://sso.garmin.com/sso");
        post_params.insert("locale", "en_US");
        post_params.insert("id", "gauth-widget");
        post_params.insert("cssUrl", "https://connect.garmin.com/gauth-custom-v1.2-min.css");
        post_params.insert("privacyStatementUrl", "https://www.garmin.com/en-US/privacy/connect/");
        post_params.insert("clientId", "GarminConnect");
        post_params.insert("rememberMeShown", "true");
        post_params.insert("rememberMeChecked", "false");
        post_params.insert("createAccountShown", "true");
        post_params.insert("openCreateAccount", "false");
        post_params.insert("displayNameShown", "false");
        post_params.insert("consumeServiceTicket", "false");
        post_params.insert("initialFocus", "true");
        post_params.insert("embedWidget", "false");
        post_params.insert("generateExtraServiceTicket", "true");
        post_params.insert("generateTwoExtraServiceTickets", "false");
        post_params.insert("generateNoServiceTicket", "false");
        post_params.insert("globalOptInShown", "true");
        post_params.insert("globalOptInChecked", "false");
        post_params.insert("mobile", "false");
        post_params.insert("connectLegalTerms", "true");
        post_params.insert("showTermsOfUse", "false");
        post_params.insert("showPrivacyPolicy", "false");
        post_params.insert("showConnectLegalAge", "false");
        post_params.insert("locationPromptShown", "true");
        post_params.insert("showPassword", "true");
        post_params.insert("useCustomHeader", "false");
        */

        let response1 = self
            .make_request(
                RequestType::GET,
                "https://sso.garmin.com/sso/signin".to_string(),
            )
            .query(&params)
            .with_context(|| "error constructing query string".to_string())?
            .send()
            .await
            .map_err(|e| anyhow!("error during pre-auth: {:?}", e))?;

        self.save_cookies(response1.headers())?;

        let mut login_data: HashMap<&str, &str> = HashMap::new();
        login_data.insert("username", &self.username);
        login_data.insert("password", &self.password);
        login_data.insert("_eventId", "submit");
        login_data.insert("embed", "true"); // was true

        let mut ret2 = self
            .make_request(
                RequestType::POST,
                "https://sso.garmin.com/sso/signin".to_string(),
            )
            .header("origin", "https://sso.garmin.com")
            .cookie(cookie::Cookie::new("GarminUserPrefs", "en-US"))
            .query(&params)
            .map_err(|e| anyhow!("error formatting query_string: {:?}", e))?
            .send_form(&login_data)
            .await
            .map_err(|e| anyhow!("error sending form: {:?}", e))?;

        let body = ret2
            .body()
            .await
            .map_err(|e| anyhow!("error getting POST response body: {:?}", e))?;

        self.save_cookies(ret2.headers())?;

        if ret2.status() != 200 {
            return Err(anyhow!(
                "error logging in, status = {}, body = {:?}",
                ret2.status(),
                body
            ));
        }

        // start the festival of redirects
        let ret3 = self
            .make_request(
                RequestType::GET,
                "https://connect.garmin.com/modern".to_string(),
            )
            .send()
            .await
            .map_err(|e| anyhow!("error sending get: {:?}", e))?;

        self.save_cookies(ret3.headers())?;

        if ret3.status() != 302 {
            return Err(anyhow!(
                "error post login, didn't get expected redirect (instead: {})",
                ret3.status()
            ));
        }

        let mut url = String::from_utf8(
            ret3.headers()
                .get("location")
                .ok_or_else(|| anyhow!("location header not present in response"))?
                .as_bytes()
                .to_vec(),
        )
        .map_err(|e| anyhow!("error finding location header in response: {:?}", e))?;

        let mut num_redirects: u8 = 1;

        while num_redirects < 7 {
            println!("login redirect #{}", num_redirects);
            // avoid getting banned
            task::sleep(Duration::from_secs(1)).await;

            let result = self
                .make_request(RequestType::GET, url)
                .send()
                .await
                .map_err(|e| anyhow!("error sending get: {:?}", e))?;

            self.save_cookies(result.headers())?;
            num_redirects +=  1;

            if result.status() == 200 {
                break;
            }

            url = String::from_utf8(
                result
                    .headers()
                    .get("location")
                    .ok_or_else(|| anyhow!("couldn't find location header in response"))?
                    .as_bytes()
                    .to_vec(),
            )
            .map_err(|e| anyhow!("error converting location header bytes to string: {:?}", e))?;
            println!("next location: {:?}", url);
        }

        self.test_auth().await?;
        self.jar.save()?;

        Ok(())
    }

    async fn get_activity_list(&mut self) -> Result<HashSet<u64>> {
        task::sleep(Duration::from_secs(1)).await;
        let mut params: HashMap<&str, String> = HashMap::new();

        let mut page: u32 = 1;
        let page_size: u32 = 100;

        let mut activities: HashSet<u64> = HashSet::new();

        loop {
            println!("fetching activity page {}", page);
            params.insert("start", ((page - 1) * page_size).to_string());
            params.insert("limit", page_size.to_string());

            let mut response = self.make_request(RequestType::GET, "https://connect.garmin.com/modern/proxy/activitylist-service/activities/search/activities".to_string())
                .query(&params).with_context(|| "error construting query string")?
                .send()
                .await
                .map_err(|e| anyhow!("error sending get request: {:?}", e))?;

            self.save_cookies(response.headers())?;

            let body = response
                .body()
                .limit(1024 * 1024 * 10) // 10MB of json?
                .await
                .map_err(|e| anyhow::Error::msg(format!("{:?}", e)))?;

            let body_str = String::from_utf8(body.to_vec()).unwrap();
            let as_json: Vec<Value> = serde_json::from_str(&body_str)?;

            if as_json.is_empty() {
                break;
            }

            for activity in as_json {
                let id = activity["activityId"].as_u64().unwrap();
                activities.insert(id);
            }

            page += 1;
        }

        println!("found {} activities total", activities.len());

        Ok(activities)
    }

    async fn download_newly_found(&mut self) -> Result<u32> {
        let all_activities = self
            .get_activity_list()
            .await
            .map_err(|e| anyhow!("error downloading activity list from GC: {:?}", e))?;

        let to_download = self.activity_downloads.not_yet_downloaded(all_activities);
        let mut num_downloaded: u32 = 0;

        for activity in to_download {
            println!("downloading activity id {}", activity);
            let activity_bytes = self
                .download_activity(activity)
                .await
                .map_err(|e| anyhow!("error downloading activity {}: {:?}", activity, e))?;

            let download_path = Path::new(&self.activity_downloads.activity_download_directory)
                .join(format!("{}.fit", activity))
                .into_boxed_path();

            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(download_path.clone())?;

            // FIT files are zipped, might as well unzip here
            let reader = std::io::Cursor::new(&activity_bytes);
            let mut zip = zip::ZipArchive::new(reader)?;
            if zip.len() > 1 {
                return Err(anyhow!(
                    "zip of activity {} has more than one file",
                    activity
                ));
            }

            let mut zipped_fit = zip.by_index(0)?;
            let mut zip_buf: Vec<u8> = vec![0; zipped_fit.size() as usize];
            zipped_fit.read_exact(&mut zip_buf)?;

            file.write_all(&zip_buf)?;
            self.activity_downloads
                .update((activity, download_path.to_str().unwrap().to_string()))?;

            num_downloaded += 1;
            println!("finished downloading activity {}", activity);
        }

        Ok(num_downloaded)
    }

    async fn download_activity(&mut self, activity_id: u64) -> Result<Vec<u8>> {
        // 	https://connect.garmin.com/modern/proxy/download-service/files/activity/5366018048

        let mut dl_response = self
            .make_request(
                RequestType::GET,
                format!(
                    "https://connect.garmin.com/modern/proxy/download-service/files/activity/{}",
                    activity_id
                ),
            )
            .send()
            .await
            .map_err(|e| anyhow!("error downloading activity {}: {:?}", activity_id, e))?;

        self.save_cookies(dl_response.headers())?;

        let body = dl_response
            .body()
            .limit(1024 * 1024 * 10)
            .await
            .map_err(|e| anyhow!("error getting body for activity {}: {:?}", activity_id, e))?;

        Ok(body.to_vec())
    }
}

#[actix_rt::main]
async fn main() {

    let matches = App::new("gliberator")
        .version("0.1")
        .author("djk121@gmail.com")
        .arg(
            Arg::with_name("gc_username")
                .help("GarminConnect username")
                .required(true)
                .index(1),
        )
        .arg(
            Arg::with_name("gc_password")
                .help("GarminConnect password")
                .required(true)
                .index(2),
        )
        .get_matches();

    let gc_username = matches.value_of("gc_username").unwrap().to_string();
    let gc_password = matches.value_of("gc_password").unwrap().to_string();

    let mut gc = match GarminConnect::new(
        gc_username,
        gc_password,
        ACTIVITY_DOWNLOAD_DIRECTORY.to_string(),
        PREVIOUSLY_DOWNLOADED_ACTIVITIES_INDEX.to_string(),
    )
    .await
    {
        Ok(gc) => gc,
        Err(e) => {
            println!("error:");
            println!("{:?}", e);
            std::process::exit(1);
        }
    };

    match gc.download_newly_found().await {
        Ok(num_downloaded) => println!("finished run, downloaded {} activities", num_downloaded),
        Err(e) => println!("error downloading activities: {:?}", e),
    }
}

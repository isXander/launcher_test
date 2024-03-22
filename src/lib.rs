use std::{collections::HashMap, path::{Path, PathBuf}};

use anyhow::Error;
use regex::Regex;
use reqwest;
use serde::Deserialize;
use sha1::{Digest, Sha1};
use text_io;
use tokio::task::JoinSet;

pub async fn launch_minecraft() -> anyhow::Result<()> {
    let client = reqwest::Client::new();
    let version_manifest = retrieve_versions(&client).await.unwrap();

    let work_path = std::env::current_dir()?.join("run");
    println!("{:?}", work_path);

    println!("Launching latest version...");
    let version = version_manifest
        .find_version_by_id(&version_manifest.latest.snapshot)
        .unwrap();
    let info = version.resolve_version_info(&client).await.unwrap();

    // download libraries
    let libraries_path = work_path.join("libraries");
    for chunked_libs in info.libraries.chunks(4) {
        let futures = chunked_libs
            .iter()
            .map(|lib| {
                let client_clone = client.clone();
                let path_clone = libraries_path.clone();

                async move {
                    let artifact = &lib.downloads.artifact;
                    download_artifact(
                        &path_clone.join(&artifact.path),
                        &artifact.info,
                        &client_clone,
                    )
                    .await
                }
            })
            .collect::<Vec<_>>();
        let results = futures::future::join_all(futures).await;

        for result in results {
            result.unwrap()
        }
    }

    // download client
    let client_jar_path = work_path.join(format!("{}.jar", info.id));
    download_artifact(&client_jar_path, &info.downloads.client, &client).await?;

    // retrieve assets
    let assets_dir = work_path.join("assets");
    let indexes_dir = assets_dir.join("indexes");
    let objects_dir = assets_dir.join("objects");

    let index_file = indexes_dir.join(format!("{}.json", &info.asset_index.id));
    download_artifact(&index_file, &info.asset_index.info, &client).await?;
    let index_json = tokio::fs::read_to_string(index_file).await?;
    let index_json: AssetIndex = serde_json::from_str(index_json.as_str())?;

    for chunked_objects in index_json
        .objects
        .values()
        .filter(|obj| {
            let hash_prefix: String = obj.hash.chars().take(2).collect();
            let asset_file = objects_dir.join(&hash_prefix).join(&obj.hash);
            !asset_file.exists()
        })
        .collect::<Vec<_>>()
        .chunks(4)
    {
        let futures = chunked_objects
            .iter()
            .map(|obj| {
                let client = client.clone();
                let hash_prefix: String = obj.hash.chars().take(2).collect();
                let asset_file = objects_dir.join(&hash_prefix).join(&obj.hash);

                async move {
                    let obj_bytes = client
                        .get(format!(
                            "https://resources.download.minecraft.net/{}/{}",
                            hash_prefix, obj.hash
                        ))
                        .send()
                        .await?
                        .bytes()
                        .await?;

                    tokio::fs::create_dir_all(&asset_file.parent().unwrap()).await?;
                    tokio::fs::write(&asset_file, obj_bytes).await?;

                    Ok(())
                }
            })
            .collect::<Vec<_>>();

        let results: Vec<Result<(), Error>> = futures::future::join_all(futures).await;
        for result in results {
            result.unwrap();
        }
    }

    let game_dir = work_path.join(".minecraft");
    std::fs::create_dir_all(&game_dir).unwrap();

    let mut classpath = info.libraries
        .iter()
        .map(|lib| {
            let path = libraries_path.join(&lib.downloads.artifact.path);
            canonicalize_and_str(&path).unwrap()
        })
        .collect::<Vec<_>>();
    classpath.push(canonicalize_and_str(&client_jar_path).unwrap());
    let classpath = classpath.join(";");
    
    println!("{}", classpath);

    let arg_query = ArgumentQuery {
        constants: HashMap::from([
            (String::from("auth_player_name"), String::from("Test")),
            (String::from("version_name"), info.id.clone()),
            (String::from("game_directory"), canonicalize_and_str(&game_dir).unwrap()),
            (String::from("assets_root"), canonicalize_and_str(&assets_dir).unwrap()),
            (String::from("assets_index_name"), info.asset_index.id.clone()),
            (String::from("auth_uuid"), String::from("fa7dae1b-e8ca-4540-9195-356e364db0af")),
            (String::from("clientid"), String::from("")),
            (String::from("auth_xuid"), String::from("")),
            (String::from("user_type"), String::from("msa")),
            (String::from("version_type"), String::from("ModLauncher")),
            (String::from("natives_directory"), canonicalize_and_str(&libraries_path).unwrap()),
            (String::from("launcher_name"), String::from("ModLauncher")),
            (String::from("launcher_version"), String::from("0.1.0")),
            (String::from("classpath"), classpath)
        ]),
        features: vec![],
        os_properties: OSProperties { name: String::from("windows"), arch: String::from("x86_64") }
    };

    let jvm_args = dbg!(resolve_arguments(info.arguments.jvm, &arg_query));
    let game_args = dbg!(resolve_arguments(info.arguments.game, &arg_query));

    let output = tokio::process::Command::new(r"C:\Users\xande\.jdks\temurin-17.0.10\bin\javaw.exe")
        .args(jvm_args)
        .arg(info.main_class)
        .args(game_args)
        .output()
        .await?;
    println!("{}", String::from_utf8(output.stdout)?);
    println!("{}", String::from_utf8(output.stderr)?);

    Ok(())
}

async fn retrieve_versions(client: &reqwest::Client) -> anyhow::Result<VersionManifest> {
    let body = client
        .get("https://piston-meta.mojang.com/mc/game/version_manifest_v2.json")
        .send()
        .await?
        .json::<VersionManifest>()
        .await?;

    Ok(body)
}

async fn download_artifact(
    path: &PathBuf,
    file_info: &FileInfo,
    client: &reqwest::Client,
) -> anyhow::Result<()> {
    if path.exists() {
        if check_sha1_matches(tokio::fs::read(&path).await?.as_slice(), &file_info.sha1) {
            return Ok(()); // no need to re-download
        }
    }

    // let head = client.head(&artifact.info.url).send().await?;
    // if head.content_length().unwrap() != artifact.info.size {
    //     panic!("Unexpected size. Got {} expected {}", head.content_length().unwrap(), artifact.info.size)
    // }

    let bytes = client.get(&file_info.url).send().await?.bytes().await?;

    if !check_sha1_matches(&bytes, &file_info.sha1) {
        panic!("Incorrect hash")
    }

    tokio::fs::create_dir_all(&path.parent().unwrap()).await?;
    tokio::fs::write(&path, bytes).await?;

    Ok(())
}

fn resolve_arguments(arguments: Vec<LaunchArgument>, arg_query: &ArgumentQuery) -> Vec<String> {
    let mut resolved = Vec::new();
    let arg_regex = Regex::new(r"\$\{(?<key>\w+)}").unwrap();
    
    for arg in arguments {
        let mut str_forms = match arg {
            LaunchArgument::String(str) => vec![str],
            LaunchArgument::Rules { rules, value } => {
                let add_arguments = rules.iter().all(|rule| {
                    let passed_features = rule.features.as_ref().map_or(true, |features|{
                        features.iter().all(|(feature, state)| arg_query.features.contains(feature) || !state)
                    });

                    let passed_os = rule.os.as_ref().map_or(true, |os| {
                        let passed_name = os.name.as_ref()
                            .map_or(true, |name| arg_query.os_properties.name == *name);
                        let passed_arch = os.arch.as_ref()
                            .map_or(true, |arch| arg_query.os_properties.arch == *arch);
                        passed_name && passed_arch
                    });
    

                    let passed = passed_features && passed_os;
                    passed != (matches!(rule.action, RuleAction::Deny))
                });
                
                if add_arguments {
                    match value {
                        RuleType::String(str) => vec![str],
                        RuleType::Array(vec) => vec
                    }
                } else {
                    vec![]
                }
            }
        };

        for arg in str_forms.iter_mut() {
            *arg = arg_regex.replace_all(arg, |caps: &regex::Captures| {
                let key = caps["key"].to_string();
                match arg_query.constants.get(&key) {
                    Some(x) => x.clone(),
                    None => {
                        println!("Could not find key {}", key);
                        String::from("")
                    }
                }
            }).into_owned();
        }

        resolved.append(&mut str_forms);
    }
    
    resolved
}

struct ArgumentQuery {
    constants: HashMap<String, String>,
    features: Vec<String>,
    os_properties: OSProperties,
}

struct OSProperties {
    name: String,
    arch: String,
}

fn check_sha1_matches(bytes: impl AsRef<[u8]>, sha1: &String) -> bool {
    let mut hasher = Sha1::new();
    hasher.update(bytes);
    let result = hasher.finalize();
    let result = format!("{:x}", result);

    &result == sha1
}

fn canonicalize_and_str(path: &PathBuf) -> anyhow::Result<String> {
    dbg!(path);
    Ok(dunce::canonicalize(path)?.into_os_string().into_string().unwrap())
    
}

#[derive(Deserialize, Debug)]
struct VersionManifest {
    latest: LatestVersion,
    versions: Vec<Version>,
}

impl VersionManifest {
    fn find_version_by_id(&self, id: &str) -> Option<&Version> {
        self.versions.iter().find(|x| x.id == id)
    }
}

#[derive(Deserialize, Debug)]
struct LatestVersion {
    release: String,
    snapshot: String,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct Version {
    id: String,
    #[serde(rename = "type")]
    vtype: VersionType,
    url: String,
    time: String,
    #[serde(with = "time::serde::iso8601")]
    release_time: time::OffsetDateTime,
    sha1: String,
    compliance_level: u8,
}

impl Version {
    async fn resolve_version_info(&self, client: &reqwest::Client) -> anyhow::Result<VersionInfo> {
        let body = client
            .get(&self.url)
            .send()
            .await?
            .json::<VersionInfo>()
            .await?;

        Ok(body)
    }
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "snake_case")]
enum VersionType {
    Release,
    Snapshot,
    OldBeta,
    OldAlpha,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct VersionInfo {
    arguments: LaunchArguments,
    asset_index: AssetIndexFile,
    assets: String,
    compliance_level: u8,
    downloads: VersionDownloads,
    id: String,
    java_version: JavaVersion,
    libraries: Vec<Library>,
    logging: LoggingConfiguration,
    main_class: String,
    minimum_launcher_version: u8,
    #[serde(with = "time::serde::iso8601")]
    release_time: time::OffsetDateTime,
    #[serde(with = "time::serde::iso8601")]
    time: time::OffsetDateTime,
    #[serde(rename = "type")]
    vtype: VersionType,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct AssetIndexFile {
    id: String,
    total_size: u64,
    #[serde(flatten)]
    info: FileInfo,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct VersionDownloads {
    client: FileInfo,
    client_mappings: Option<FileInfo>,
    server: Option<FileInfo>,
    server_mappings: Option<FileInfo>,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct JavaVersion {
    component: String,
    major_version: u8,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct Library {
    downloads: LibraryDownloads,
    name: String,
    rules: Option<Vec<Rule>>,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct LibraryDownloads {
    artifact: Artifact,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct Artifact {
    path: String,
    #[serde(flatten)]
    info: FileInfo,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct LoggingConfiguration {
    client: Option<SidedLoggingConfiguration>,
    server: Option<SidedLoggingConfiguration>,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct SidedLoggingConfiguration {
    argument: String,
    file: File,
    #[serde(rename = "type")]
    ltype: String,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct File {
    id: String,
    #[serde(flatten)]
    info: FileInfo,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct FileInfo {
    sha1: String,
    size: u64,
    url: String,
}

#[derive(Deserialize, Debug)]
struct LaunchArguments {
    game: Vec<LaunchArgument>,
    jvm: Vec<LaunchArgument>,
}

#[derive(Deserialize, Debug)]
#[serde(untagged)]
enum LaunchArgument {
    String(String),
    Rules { rules: Vec<Rule>, value: RuleType },
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "snake_case")]
enum RuleAction {
    Allow,
    Deny,
}

#[derive(Deserialize, Debug)]
#[serde(untagged)]
enum RuleType {
    String(String),
    Array(Vec<String>),
}

#[derive(Deserialize, Debug)]
struct Rule {
    action: RuleAction,
    features: Option<HashMap<String, bool>>,
    os: Option<ArgumentRuleOSConstraint>,
}

#[derive(Deserialize, Debug)]
struct ArgumentRuleOSConstraint {
    name: Option<String>,
    arch: Option<String>,
}

#[derive(Deserialize)]
struct AssetIndex {
    objects: HashMap<String, Asset>,
}

#[derive(Deserialize)]
struct Asset {
    hash: String,
    size: u64,
}

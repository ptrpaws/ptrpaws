use chrono::prelude::*;
use rayon::prelude::*;
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::env;
use std::fs::File;
use std::io::Write;
use tera::{Context, Tera};

#[derive(Deserialize, Debug)]
struct GraphQLResponse {
    data: Option<Data>,
    errors: Option<Vec<serde_json::Value>>,
}
#[derive(Deserialize, Debug)]
struct Data {
    user: User,
}
#[derive(Deserialize, Debug)]
struct User {
    #[serde(rename = "contributionsCollection")]
    contributions_collection: ContributionsCollection,
    #[serde(rename = "pullRequests")]
    pull_requests: TotalCount,
    issues: TotalCount,
    repositories: Repositories,
    #[serde(rename = "repositoriesContributedTo")]
    repositories_contributed_to: TotalCount,
}
#[derive(Deserialize, Debug)]
struct ContributionsCollection {
    #[serde(rename = "totalCommitContributions")]
    total_commit_contributions: u64,
    #[serde(rename = "restrictedContributionsCount")]
    restricted_contributions_count: u64,
}
#[derive(Deserialize, Debug)]
struct TotalCount {
    #[serde(rename = "totalCount")]
    total_count: u64,
}
#[derive(Deserialize, Debug)]
struct Repositories {
    nodes: Vec<Stargazer>,
}
#[derive(Deserialize, Debug)]
struct Stargazer {
    #[serde(rename = "stargazerCount")]
    stargazer_count: u64,
}

fn query_user_stats(username: &str, token: &str) -> Result<User, Box<dyn std::error::Error>> {
    let client = Client::new();
    let now = Utc::now();
    let beginning_of_year = Utc.with_ymd_and_hms(now.year(), 1, 1, 0, 0, 0).unwrap();
    let end_of_year = Utc.with_ymd_and_hms(now.year(), 12, 31, 23, 59, 59).unwrap();

    let query = r#"
        query($username: String!, $from: DateTime, $to: DateTime) {
          user(login: $username) {
            contributionsCollection(from: $from, to: $to) {
              totalCommitContributions
              restrictedContributionsCount
            }
            pullRequests { totalCount }
            issues { totalCount }
            repositories(first: 100, ownerAffiliations: OWNER, isFork: false) {
              nodes { stargazerCount }
            }
            repositoriesContributedTo(first: 1, contributionTypes: [COMMIT, ISSUE, PULL_REQUEST, REPOSITORY]) {
              totalCount
            }
          }
        }
    "#;

    let response = client
        .post("https://api.github.com/graphql")
        .header("Authorization", format!("Bearer {}", token))
        .header("User-Agent", "Rust GitHub README Generator")
        .json(&json!({
            "query": query,
            "variables": { "username": username, "from": beginning_of_year.to_rfc3339(), "to": end_of_year.to_rfc3339() }
        }))
        .send()?;

    if !response.status().is_success() {
        return Err(format!("GitHub API returned non-success status: {}", response.text()?).into());
    }

    let gql_response: GraphQLResponse = response.json()?;
    if gql_response.errors.is_some() {
        return Err("GraphQL query failed.".into());
    }

    Ok(gql_response
        .data
        .ok_or("Missing 'data' field in GraphQL response")?
        .user)
}

fn calculate_language_stats(
    _username: &str,
    token: &str,
) -> Result<Vec<(String, f64)>, Box<dyn std::error::Error>> {
    let client = Client::new();
    let mut all_repos: Vec<serde_json::Value> = Vec::new();
    let mut page = 1;

    loop {
        let url = format!(
            "https://api.github.com/user/repos?type=owner&per_page=100&page={}",
            page
        );
        let response = client
            .get(&url)
            .header("Authorization", format!("token {}", token))
            .header("User-Agent", "Rust GitHub README Generator")
            .send()?;
        let mut repos: Vec<serde_json::Value> = response.json()?;
        if repos.is_empty() {
            break;
        }
        all_repos.append(&mut repos);
        page += 1;
    }

    let lang_maps: Vec<HashMap<String, u64>> = all_repos
        .par_iter()
        .filter_map(|repo| {
            if repo["fork"].as_bool().unwrap_or(false) {
                return None;
            }
            if let Some(topics) = repo["topics"].as_array() {
                // ** NEW FILTER **: Skip if the repo has the `mirror` or `no-stats` topic.
                if topics.iter().any(|t| t.as_str() == Some("mirror") || t.as_str() == Some("no-stats")) {
                    return None;
                }
            }
            repo["languages_url"].as_str().and_then(|url| {
                client
                    .get(url)
                    .header("Authorization", format!("token {}", token))
                    .header("User-Agent", "Rust GitHub README Generator")
                    .send()
                    .and_then(|resp| resp.json::<HashMap<String, u64>>())
                    .ok()
            })
        })
        .collect();

    let mut languages = HashMap::new();
    for map in lang_maps {
        for (lang, bytes) in map {
            *languages.entry(lang).or_insert(0) += bytes;
        }
    }

    let total_bytes: u64 = languages.values().sum();
    if total_bytes == 0 {
        return Ok(Vec::new());
    }

    let mut language_percentages: Vec<(String, f64)> = languages
        .into_iter()
        .map(|(lang, count)| (lang, (count as f64 / total_bytes as f64) * 100.0))
        .collect();

    language_percentages.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
    language_percentages.truncate(8);
    Ok(language_percentages)
}

fn abbreviate_number(n: u64) -> String {
    if n >= 1000 {
        format!("{:.1}k", (n as f64) / 1000.0)
    } else {
        n.to_string()
    }
}

fn render_progress_bar(percentage: f64) -> String {
    let num_filled = (percentage / 10.0).round().max(0.0) as usize;
    let num_empty = (10 - num_filled).max(0);
    format!("{}{}", "▓".repeat(num_filled), "░".repeat(num_empty))
}

fn format_lang_name(lang: &str) -> String {
    match lang {
        "Visual Basic .NET" => "VB.NET".to_string(),
        "Jupyter Notebook" => "Jupyter".to_string(),
        _ => lang.to_string(),
    }
}

#[derive(Serialize)]
struct TemplateLanguage {
    name: String,
    bar: String,
    percentage_str: String,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let username = "ptrpaws";
    let token = env::var("GH_PAT").expect("GH_PAT not set");

    let user_stats = query_user_stats(username, &token)?;
    let top_languages = calculate_language_stats(username, &token)?;

    let total_stars: u64 = user_stats.repositories.nodes.iter().map(|repo| repo.stargazer_count).sum();
    let total_commits_this_year = user_stats.contributions_collection.total_commit_contributions + user_stats.contributions_collection.restricted_contributions_count;

    let display_langs: Vec<TemplateLanguage> = top_languages
        .into_iter()
        .map(|(lang, percentage)| TemplateLanguage {
            name: format!("{:<15}", format_lang_name(&lang)),
            bar: render_progress_bar(percentage),
            percentage_str: format!("{:.2}%", percentage),
        })
        .collect();

    let tera = Tera::new("templates/**/*.tera")?;
    let mut context = Context::new();

    context.insert("username", &username);
    context.insert("total_stars", &abbreviate_number(total_stars));
    context.insert("total_commits_this_year", &abbreviate_number(total_commits_this_year));
    context.insert("total_prs", &abbreviate_number(user_stats.pull_requests.total_count));
    context.insert("total_issues", &abbreviate_number(user_stats.issues.total_count));
    context.insert("contributed_to", &abbreviate_number(user_stats.repositories_contributed_to.total_count));
    context.insert("languages", &display_langs);
    context.insert("last_updated", &format!("Last updated {} UTC", Utc::now().format("%Y-%m-%d %H:%M:%S")));

    let readme_content = tera.render("README.md.tera", &context)?;

    let mut file = File::create("README.md")?;
    file.write_all(readme_content.as_bytes())?;

    Ok(())
}
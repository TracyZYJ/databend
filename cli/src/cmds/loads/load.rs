// Copyright 2020 Datafuse Labs.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::borrow::Borrow;
use std::io::{Read, BufRead};
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use clap::App;
use clap::AppSettings;
use clap::Arg;
use clap::ArgMatches;
use comfy_table::Cell;
use comfy_table::Color;
use comfy_table::Table;
use common_base::ProgressValues;
use lexical_util::num::AsPrimitive;
use num_format::Locale;
use num_format::ToFormattedString;
use itertools::Itertools;
use crate::cmds::clusters::cluster::ClusterProfile;
use crate::cmds::command::Command;
use crate::cmds::Config;
use crate::cmds::Status;
use crate::cmds::Writer;
use crate::error::CliError;

use crate::error::Result;
use std::str::FromStr;
use std::iter::Map;
use databend_query::common::HashMap;
use std::collections::BTreeMap;
use crate::cmds::queries::query::{build_query_endpoint, execute_query_json};
use reqwest::Client;
use common_base::tokio::io::{BufReader, AsyncBufReadExt, AsyncRead};
use rayon::prelude::*;
use futures::StreamExt;
use common_base::tokio::fs::File;

// Support different file format to be loaded
pub enum FileFormat {
    CSV,
}

impl FromStr for FileFormat {
    type Err = &'static str;

    fn from_str(s: &str) -> std::result::Result<FileFormat, &'static str> {
        match s {
            "csv" => Ok(FileFormat::CSV),
            _ => Err("no match for profile"),
        }
    }
}

pub struct Schema {
    schema: BTreeMap<String, String>
}

impl FromStr for Schema {
    type Err = &'static str;

    fn from_str(s: &str) -> std::result::Result<Schema, &'static str> {
        let mut str = String::from(s);
        let mut schema = Schema{ schema: BTreeMap::new() };
        str.retain(|e| e != ' ');
        for field in str.split(","){
            let elems :Vec<&str> = field.split(':').filter(|e| !e.is_empty() ).collect();
            if elems.len() != 2 {
                return Err("not a valid schema, please input schema in format like a:uint8,b:uint64")
            }
            schema.schema.insert(elems[0].to_string(), elems[1].to_string());
        }
        Ok(schema)
    }
}

impl ToString for Schema {
    fn to_string(&self) -> String {
        return self.schema.iter().map(|(a,b) | a.to_owned() + " " + &*b.to_owned()).join(",")
    }
}

#[derive(Clone)]
pub struct LoadCommand {
    #[allow(dead_code)]
    conf: Config,
    clap: App<'static>,
}

impl LoadCommand {
    pub fn create(conf: Config) -> Self {
        let clap = LoadCommand::generate();
        LoadCommand { conf, clap }
    }
    pub fn generate() -> App<'static> {
        let app = App::new("load")
            .setting(AppSettings::DisableVersionFlag)
            .about("Query on databend cluster")
            .arg(
                Arg::new("profile")
                    .long("profile")
                    .about("Profile to run queries")
                    .required(false)
                    .possible_values(&["local"])
                    .default_value("local"),
            )
            .arg(
                Arg::new("format").long("format")
                    .about("the format of file, support csv")
                    .takes_value(true)
                    .required(false)
                    .default_value("csv"),
            )
            .arg(
                Arg::new("schema").long("schema")
                    .about("defined schema for table load, for example:\
                    bendctl load --schema a:uint8, b:uint64, c:String")
                    .takes_value(true)
                    .required(false),
            )
            .arg(
                Arg::new("load")
                    .about("file to get loaded for example foo.csv")
                    .takes_value(true)
                    .required(false),
            )
            .arg(
                Arg::new("skip-head-lines").long("skip-head-lines")
                    .about("skip head line in file for example: \
                    bendctl load test.csv --skip-head-lines 10 would ignore the first ten lines in csv file")
                    .takes_value(true)
                    .required(false),
            )
            .arg(
                Arg::new("table").long("table")
                .about("database table")
                .takes_value(true)
                .required(true),
            );

        app
    }
    async fn local_exec_match(&self, writer: &mut Writer, args: &ArgMatches) -> Result<()> {
        match self.local_exec_precheck(args).await {
            Ok(_) => {
                 match args.value_of("load") {
                    Some(val) => {
                        if Path::new(val).exists() {
                            let buffer =
                                std::fs::read(Path::new(val)).expect("cannot read query from file");
                            String::from_utf8_lossy(&*buffer).to_string();
                        }
                    }
                    None => {
                        let io = common_base::tokio::io::stdin();
                        let mut reader = BufReader::new(io).lines();
                        for i in 0..args.value_of("skip-head-lines").unwrap_or("0").parse::<usize>().unwrap() {
                            if let None = reader.next_line().await? {
                                return Ok(())
                            }
                        }
                        let table = args.value_of("table").unwrap();
                        let schema = args.value_of("schema");
                        let table_format = match schema {
                            Some(s) => {
                                let schema : Schema = args.value_of_t("schema").expect("cannot build schema");
                                format!("{} ({})", table, schema.schema.keys().into_iter().join(", "))
                            }
                            None => {
                                table.to_string()
                            }
                        };
                        let status = Status::read(self.conf.clone())?;
                        let (cli, url) = build_query_endpoint(&status)?;
                        loop {
                            let mut batch = vec![];
                            for _ in 0..100_000 {
                                if let Some(line) = reader.next_line().await? {
                                    batch.push(line);
                                } else {
                                    break;
                                }
                            }
                            if batch.is_empty() {
                                break;
                            }
                            let values = batch.into_iter().par_bridge().map(|e| format!("({})", e.trim())).filter(|e| !e.trim().is_empty() ).reduce_with(|a, b | format!("{}, {}", a, b));
                            if let Some(values) = values {
                                let query = format!("INSERT INTO {} VALUES {}", table_format, values);
                                if let Err(e) = execute_query_json(&cli, &url, query).await {
                                    writer.write_err(format!("cannot insert data into {}, error: {:?}", table, e))
                                }
                            }

                        }
                    }
                };
                Ok(())
            }
            Err(e) => {
                writer.write_err(format!("Query command precheck failed, error {:?}", e));
                Ok(())
            }
        }
    }

    /// precheck would at build up and validate schema for incoming INSERT operations
    async fn local_exec_precheck(&self, args: &ArgMatches) -> Result<()> {
        let status = Status::read(self.conf.clone())?;
        if status.current_profile.is_none() {
            return Err(CliError::Unknown(format!(
                "Query command error: cannot find local configs in {}, please run `bendctl cluster create` to create a new local cluster or '\\admin' switch to the admin mode",
                status.local_config_dir
            )));
        }
        let status = Status::read(self.conf.clone())?;
        // TODO typecheck
        if args.value_of("schema").is_none() {
            if let Err(e) = table_exists(&status, args.value_of("table")).await {
                return Err(e)
            }
            Ok(())
        } else {
            match args.value_of_t::<Schema>("schema") {
                Ok(schema) => {
                    return create_table_if_not_exists(&status,args.value_of("table"), schema).await
                }
                Err(e) => {
                    return Err(CliError::Unknown(format!("{} schema is not in valid format",  args.value_of("table").unwrap())))
                }
            }
        }
    }
}

async fn build_reader<R>(load: Option<&str>) -> BufReader<R>
where R: AsyncRead
{
    match load {
        Some(val) => {
            if Path::new(val).exists() {
                let f = File::open(val).await.expect("cannot open file: permission denied");
                return BufReader::new(f)
            } else if val.starts_with("http://") || val.starts_with("https://") {
                let res = reqwest::get(val)
                    .await
                    .expect("cannot fetch query from url")
                    .text()
                    .await
                    .expect("cannot fetch response body");
                res
            } else {
                val.to_string()
            }
        }
        None => {
            let io = common_base::tokio::io::stdin();
            return BufReader::new(io)
        }
    }
    BufReader::new(io)
}

async fn table_exists(status: &Status, table: Option<&str>) -> Result<()>  {
    match table {
        Some(t) => {
            let (cli, url) = build_query_endpoint(status)?;
            let query = format!("SHOW TABLES LIKE '{}';", t);
            let (col, data, _) = execute_query_json(&cli, &url, query).await?;
            if col.is_none() || data.is_none() || data.unwrap().is_empty() {
                return Err(CliError::Unknown(format!("table {} not found", t)))
            }
        }
        None => {
            return Err(CliError::Unknown("no table found in argument".to_string()))
        }
    }
    Ok(())
}

async fn create_table_if_not_exists(status: &Status, table: Option<&str>, schema: Schema) -> Result<()> {
    return match table_exists(status, table).await {
        Ok(_) => {
            Ok(())
        }
        Err(_) => {
            let (cli, url) = build_query_endpoint(status)?;
            let query = format!("CREATE TABLE {}({}) Engine = Fuse;", table.unwrap(), schema.to_string());
            execute_query_json(&cli, &url, query).await?;
            Ok(())
        }
    }
}

#[async_trait]
impl Command for LoadCommand {
    fn name(&self) -> &str {
        "load"
    }

    fn clap(&self) -> App<'static> {
        self.clap.clone()
    }

    fn about(&self) -> &'static str {
        "Query on databend cluster"
    }

    fn is(&self, s: &str) -> bool {
        s.contains(self.name())
    }

    fn subcommands(&self) -> Vec<Arc<dyn Command>> {
        vec![]
    }

    async fn exec_matches(&self, writer: &mut Writer, args: Option<&ArgMatches>) -> Result<()> {
        match args {
            Some(matches) => {
                let profile = matches.value_of_t("profile");
                match profile {
                    Ok(ClusterProfile::Local) => {
                        return self.local_exec_match(writer, matches).await;
                    }
                    Ok(ClusterProfile::Cluster) => {
                        todo!()
                    }
                    Err(_) => writer
                        .write_err("Currently profile only support cluster or local".to_string()),
                }
            }
            None => {
                println!("none ");
            }
        }
        Ok(())
    }
}

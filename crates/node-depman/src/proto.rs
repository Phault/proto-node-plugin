use crate::npm_registry::parse_registry_response;
use crate::package_manager::PackageManager;
use extism_pdk::*;
use node_common::{
    commands::{self, get_global_prefix},
    NodeDistVersion, PackageJson, PluginConfig,
};
use proto_pdk::*;
use std::collections::HashMap;

#[host_fn]
extern "ExtismHost" {
    fn exec_command(input: Json<ExecCommandInput>) -> Json<ExecCommandOutput>;
    fn get_env_var(key: &str) -> String;
}

#[plugin_fn]
pub fn register_tool(Json(_): Json<ToolMetadataInput>) -> FnResult<Json<ToolMetadataOutput>> {
    let manager = PackageManager::detect()?;

    Ok(Json(ToolMetadataOutput {
        name: manager.to_string(),
        type_of: PluginType::DependencyManager,
        default_version: if manager == PackageManager::Npm {
            Some(UnresolvedVersionSpec::Alias("bundled".into()))
        } else {
            None
        },
        plugin_version: Some(env!("CARGO_PKG_VERSION").into()),
        ..ToolMetadataOutput::default()
    }))
}

#[plugin_fn]
pub fn detect_version_files(_: ()) -> FnResult<Json<DetectVersionOutput>> {
    Ok(Json(DetectVersionOutput {
        files: vec!["package.json".into()],
        ignore: vec!["node_modules".into()],
    }))
}

#[plugin_fn]
pub fn parse_version_file(
    Json(input): Json<ParseVersionFileInput>,
) -> FnResult<Json<ParseVersionFileOutput>> {
    let mut version = None;

    if input.file == "package.json" {
        if let Ok(package_json) = json::from_str::<PackageJson>(&input.content) {
            let manager_name = PackageManager::detect()?.to_string();

            if let Some(pm) = package_json.package_manager {
                let mut parts = pm.split('@');
                let name = parts.next().unwrap_or_default();

                if name == manager_name {
                    let value = if let Some(value) = parts.next() {
                        // Remove corepack build metadata hash
                        if let Some(index) = value.find('+') {
                            &value[0..index]
                        } else {
                            value
                        }
                    } else {
                        "latest"
                    };

                    version = Some(UnresolvedVersionSpec::parse(value)?);
                }
            }

            if version.is_none() {
                if let Some(engines) = package_json.engines {
                    if let Some(constraint) = engines.get(&manager_name) {
                        version = Some(UnresolvedVersionSpec::parse(constraint)?);
                    }
                }
            }

            if version.is_none() {
                if let Some(volta) = package_json.volta {
                    if let Some(volta_tool_version) = match manager_name.as_str() {
                        "npm" => volta.npm,
                        "pnpm" => volta.pnpm,
                        "yarn" => volta.yarn,
                        _ => None,
                    } {
                        version = Some(UnresolvedVersionSpec::parse(volta_tool_version)?);
                    }
                }
            }
        }
    }

    Ok(Json(ParseVersionFileOutput { version }))
}

#[plugin_fn]
pub fn load_versions(Json(input): Json<LoadVersionsInput>) -> FnResult<Json<LoadVersionsOutput>> {
    let mut output = LoadVersionsOutput::default();
    let manager = PackageManager::detect()?;
    let package_name = manager.get_package_name(&input.initial);

    let mut map_output = |res: HttpResponse, is_yarn: bool| -> Result<(), Error> {
        let res = parse_registry_response(res, is_yarn)?;

        for item in res.versions.values() {
            output.versions.push(Version::parse(&item.version)?);
        }

        // Dist tags always includes latest
        for (alias, version) in res.dist_tags {
            let version = Version::parse(&version)?;

            if alias == "latest" && output.latest.is_none() {
                output.latest = Some(version.clone());
            }

            output.aliases.entry(alias).or_insert(version);
        }

        Ok(())
    };

    // Yarn is managed by 2 different packages, so we need to request versions from both of them!
    if manager == PackageManager::Yarn {
        map_output(
            fetch(HttpRequest::new("https://registry.npmjs.org/yarn/"), None)?,
            true,
        )?;
        map_output(
            fetch(
                HttpRequest::new("https://registry.npmjs.org/@yarnpkg/cli-dist/"),
                None,
            )?,
            true,
        )?;
    } else {
        map_output(
            fetch(
                HttpRequest::new(format!("https://registry.npmjs.org/{}/", package_name)),
                None,
            )?,
            false,
        )?;
    }

    output
        .aliases
        .insert("latest".into(), output.latest.clone().unwrap());

    Ok(Json(output))
}

#[plugin_fn]
pub fn resolve_version(
    Json(input): Json<ResolveVersionInput>,
) -> FnResult<Json<ResolveVersionOutput>> {
    let manager = PackageManager::detect()?;
    let mut output = ResolveVersionOutput::default();

    match manager {
        PackageManager::Npm => {
            // When the alias "bundled" is provided, we should install the npm
            // version that comes bundled with the current Node.js version.
            if input.initial.is_alias("bundled") {
                debug!("Received the bundled alias, attempting to find a version");

                let response: Vec<NodeDistVersion> =
                    fetch_url("https://nodejs.org/download/release/index.json")?;
                let mut found_version = false;

                // Infer from proto's environment variable
                if let Some(node_version) = host_env!("PROTO_NODE_VERSION") {
                    for node_release in &response {
                        // Theirs starts with v, ours does not
                        if node_release.version[1..] == node_version && node_release.npm.is_some() {
                            output.version =
                                Some(VersionSpec::parse(node_release.npm.as_ref().unwrap())?);
                            found_version = true;
                            break;
                        }
                    }
                }

                // Otherwise call the current `node` binary and infer from that
                if !found_version {
                    let result = exec_command!("node", ["--version"]);
                    let node_version = result.stdout.trim();

                    for node_release in &response {
                        // Both start with v
                        if node_release.version == node_version && node_release.npm.is_some() {
                            output.version =
                                Some(VersionSpec::parse(node_release.npm.as_ref().unwrap())?);
                            found_version = true;
                            break;
                        }
                    }
                }

                if !found_version {
                    debug!(
                        "Could not find a bundled npm version for Node.js, falling back to latest"
                    );

                    output.candidate = Some(UnresolvedVersionSpec::Alias("latest".into()));
                }
            }
        }

        PackageManager::Yarn => {
            if let UnresolvedVersionSpec::Alias(alias) = input.initial {
                if alias == "berry" || alias == "latest" {
                    output.candidate = Some(UnresolvedVersionSpec::parse("~4")?);
                } else if alias == "legacy" || alias == "classic" {
                    output.candidate = Some(UnresolvedVersionSpec::parse("~1")?);
                }
            }
        }

        _ => {}
    };

    Ok(Json(output))
}

fn get_archive_prefix(manager: &PackageManager, spec: &VersionSpec) -> String {
    if manager.is_yarn_classic(spec.to_unresolved_spec()) {
        if let VersionSpec::Version(version) = spec {
            // Prefix changed to "package" in v1.22.20
            // https://github.com/yarnpkg/yarn/releases/tag/v1.22.20
            if version.minor <= 22 && version.patch <= 19 {
                return format!("yarn-v{version}");
            }
        }
    }

    "package".into()
}

#[plugin_fn]
pub fn download_prebuilt(
    Json(input): Json<DownloadPrebuiltInput>,
) -> FnResult<Json<DownloadPrebuiltOutput>> {
    let version = &input.context.version;
    let manager = PackageManager::detect()?;

    if version.is_canary() {
        return Err(plugin_err!(PluginError::UnsupportedCanary {
            tool: manager.to_string()
        }));
    }

    let package_name = manager.get_package_name(version.to_unresolved_spec());

    let package_without_scope = if package_name.contains('/') {
        package_name.split('/').nth(1).unwrap()
    } else {
        &package_name
    };

    Ok(Json(DownloadPrebuiltOutput {
        archive_prefix: Some(get_archive_prefix(&manager, version)),
        download_url: format!(
            "https://registry.npmjs.org/{package_name}/-/{package_without_scope}-{version}.tgz",
        ),
        ..DownloadPrebuiltOutput::default()
    }))
}

#[plugin_fn]
pub fn locate_executables(
    Json(_): Json<LocateExecutablesInput>,
) -> FnResult<Json<LocateExecutablesOutput>> {
    let manager = PackageManager::detect()?;
    let mut secondary = HashMap::default();
    let mut primary;

    // We don't link binaries for package managers for the following reasons:
    // 1 - We can't link JS files because they aren't executable.
    // 2 - We can't link the bash/cmd wrappers, as they expect the files to exist
    //     relative from the node install directory, which they do not.

    match &manager {
        PackageManager::Npm => {
            primary = ExecutableConfig::with_parent("bin/npm-cli.js", "node");
            primary.no_bin = true;

            // npx
            let mut npx = ExecutableConfig::with_parent("bin/npx-cli.js", "node");
            npx.no_bin = true;

            secondary.insert("npx".into(), npx);

            // node-gyp
            let mut node_gyp =
                ExecutableConfig::with_parent("node_modules/node-gyp/bin/node-gyp.js", "node");
            node_gyp.no_bin = true;

            secondary.insert("node-gyp".into(), node_gyp);
        }
        PackageManager::Pnpm => {
            primary = ExecutableConfig::with_parent("bin/pnpm.cjs", "node");
            primary.no_bin = true;

            // pnpx
            secondary.insert(
                "pnpx".into(),
                ExecutableConfig {
                    no_bin: true,
                    shim_before_args: Some(StringOrVec::String("dlx".into())),
                    ..ExecutableConfig::default()
                },
            );
        }
        PackageManager::Yarn => {
            primary = ExecutableConfig::with_parent("bin/yarn.js", "node");
            primary.no_bin = true;

            // yarnpkg
            secondary.insert("yarnpkg".into(), primary.clone());
        }
    };

    Ok(Json(LocateExecutablesOutput {
        globals_lookup_dirs: vec!["$PROTO_HOME/tools/node/globals/bin".into()],
        primary: Some(primary),
        secondary,
        ..LocateExecutablesOutput::default()
    }))
}

#[plugin_fn]
pub fn install_global(
    Json(input): Json<InstallGlobalInput>,
) -> FnResult<Json<InstallGlobalOutput>> {
    let env = get_host_environment()?;

    let result = exec_command!(
        input,
        commands::install_global(
            &input.dependency,
            get_global_prefix(&env, &input.globals_dir),
        )
    );

    Ok(Json(InstallGlobalOutput::from_exec_command(result)))
}

#[plugin_fn]
pub fn uninstall_global(
    Json(input): Json<UninstallGlobalInput>,
) -> FnResult<Json<UninstallGlobalOutput>> {
    let env = get_host_environment()?;

    let result = exec_command!(
        input,
        commands::uninstall_global(
            &input.dependency,
            get_global_prefix(&env, &input.globals_dir),
        )
    );

    Ok(Json(UninstallGlobalOutput::from_exec_command(result)))
}

#[plugin_fn]
pub fn pre_run(Json(input): Json<RunHook>) -> FnResult<()> {
    let args = &input.passthrough_args;
    let config = get_tool_config::<PluginConfig>()?;

    if args.len() < 3 || !config.intercept_globals || host_env!("PROTO_INSTALL_GLOBAL").is_some() {
        return Ok(());
    }

    let manager = PackageManager::detect()?;
    let mut is_install_command = false;
    let mut is_global = false;

    // npm install -g <dep>
    // pnpm add -g <dep>
    if manager == PackageManager::Npm || manager == PackageManager::Pnpm {
        is_install_command = args[0] == "install" || args[0] == "i" || args[0] == "add";

        for arg in args {
            if arg == "--global" || arg == "-g" || arg == "--location=global" {
                is_global = true;
                break;
            }
        }
    }

    // yarn global add <dep>
    if manager == PackageManager::Yarn {
        is_global = args[0] == "global";
        is_install_command = args[1] == "add";
    }

    if is_install_command && is_global {
        return Err(plugin_err!(
            "Global binaries must be installed with <shell>proto install-global {}</shell>!\nLearn more: <url>{}</url>\n\nOpt-out of this functionality with <property>{}</property>.",
            manager.to_string(),
            "https://github.com/moonrepo/node-plugin#configuration",
            "tools.node.intercept-globals = false",
        ));
    }

    Ok(())
}

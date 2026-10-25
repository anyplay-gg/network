//! With the `sdk` feature: makes Rust bindings from Discord's header, into the build directory.
//! The header belongs to the closed-source SDK, so neither it nor the bindings are ever committed.

fn main() {
    println!("cargo:rerun-if-env-changed=DISCORD_SDK_DIR");
    println!("cargo:rerun-if-changed=build.rs");
    #[cfg(feature = "sdk")]
    generate();
}

#[cfg(feature = "sdk")]
fn generate() {
    use std::path::PathBuf;

    let Some(dir) = std::env::var_os("DISCORD_SDK_DIR") else {
        panic!(
            "the `sdk` feature needs DISCORD_SDK_DIR: the folder of Discord's Social SDK that holds \
             include/cdiscord.h (see docs/discord-plan.md). Build without the feature to use the fake."
        );
    };
    let header = PathBuf::from(dir).join("include").join("cdiscord.h");
    println!("cargo:rerun-if-changed={}", header.display());
    // Only what sdk.rs calls: the generated struct loads exactly these symbols from the library,
    // and a library that lacks one is refused when it is loaded.
    let wanted = "Discord_(RunCallbacks|Free|Client_(Init|Drop|SetApplicationId|Authorize|AbortAuthorize|\
CreateAuthorizationCodeVerifier|UpdateToken|Connect|Disconnect|GetStatus|SetStatusChangedCallback|\
GetCurrentUserV2|GetRelationships|SetRelationshipCreatedCallback|SetRelationshipDeletedCallback|\
SetRelationshipGroupsUpdatedCallback|AddLogCallback|GetVersionMajor|GetVersionMinor|GetVersionPatch)|\
ClientResult_(Drop|Successful|Error|ErrorCode)|AuthorizationCodeVerifier_(Drop|Challenge|Verifier)|\
AuthorizationCodeChallenge_Drop|AuthorizationArgs_(Init|Drop|SetClientId|SetScopes|SetCodeChallenge)|\
UserHandle_(Drop|Id|DisplayName)|RelationshipHandle_(Drop|DiscordRelationshipType))";
    let bindings = bindgen::Builder::default()
        .header(header.to_string_lossy())
        .allowlist_function(wanted)
        .allowlist_type("Discord_(Client_Status|Client_Error|LoggingSeverity|AuthorizationTokenType|RelationshipType)")
        .dynamic_library_name("Sdk")
        .dynamic_link_require_all(true)
        .default_enum_style(bindgen::EnumVariation::Consts)
        .prepend_enum_name(false)
        .layout_tests(false)
        .generate_comments(false)
        .generate()
        .expect("bindgen could not read the SDK header (is libclang installed?)");
    let out = PathBuf::from(std::env::var_os("OUT_DIR").expect("cargo sets OUT_DIR"));
    bindings
        .write_to_file(out.join("bindings.rs"))
        .expect("the bindings are written to the build directory");
}

{
  pkgs,
  craneLib,
  cargoSrc,
  runtimeAssetsSrc,
  frontendSrc,
}: let
  inherit (pkgs) lib onnxruntime stdenv;

  # Read version from Cargo.toml
  cargoToml = fromTOML (builtins.readFile "${cargoSrc}/Cargo.toml");
  inherit (cargoToml.package) version;

  buildInputs = with pkgs; [
    protobuf
    cmake
    openssl
    pkg-config
    onnxruntime
  ];

  nativeBuildInputs = with pkgs; [
    pkg-config
    protobuf
    cmake
  ] ++ lib.optionals stdenv.isLinux [pkgs.mold];

  frontendPackageLock = lib.importJSON "${frontendSrc}/interface/package-lock.json";
  frontendPackage = let
    originalPackage = lib.importJSON "${frontendSrc}/interface/package.json";
    rootDependencies = originalPackage.dependencies or {};
    lockPackages = frontendPackageLock.packages;
    rootDependencyNames = builtins.attrNames rootDependencies;

    collectPeerClosure = dependencyNames: let
      peerNames =
        lib.unique
        (builtins.concatLists (
          map (
            dependencyName: let
              packagePath = "node_modules/${dependencyName}";
              packageEntry =
                if builtins.hasAttr packagePath lockPackages
                then lockPackages.${packagePath}
                else {};
            in
              builtins.attrNames (packageEntry.peerDependencies or {})
          )
          dependencyNames
        ));

      expandedNames = lib.unique (dependencyNames ++ peerNames);
    in
      if builtins.length expandedNames == builtins.length dependencyNames
      then dependencyNames
      else collectPeerClosure expandedNames;

    peerDependencyNames =
      lib.filter (dependencyName: !(lib.elem dependencyName rootDependencyNames))
      (collectPeerClosure rootDependencyNames);

    peerDependencyVersions = builtins.listToAttrs (
      lib.filter (entry: entry != null) (
        map (
          dependencyName: let
            packagePath = "node_modules/${dependencyName}";
          in
            if builtins.hasAttr packagePath lockPackages
            then {
              name = dependencyName;
              value = lockPackages.${packagePath}.version;
            }
            else null
        )
        peerDependencyNames
      )
    );
  in
    originalPackage
    // {
      dependencies = rootDependencies // peerDependencyVersions;
    };

  frontend = pkgs.buildNpmPackage {
    inherit (pkgs.importNpmLock) npmConfigHook;
    inherit version;

    pname = "spacebot-frontend";
    src = "${frontendSrc}/interface";

    npmDeps = pkgs.importNpmLock {
      npmRoot = "${frontendSrc}/interface";
      package = frontendPackage;
      packageLock = frontendPackageLock;
    };
    npmInstallFlags = ["--legacy-peer-deps"];
    makeCacheWritable = true;

    installPhase = ''
      mkdir -p $out
      cp -r dist/* $out/
    '';
  };

  commonArgs = {
    src = cargoSrc;
    inherit nativeBuildInputs buildInputs;
    strictDeps = true;
    cargoExtraArgs = "";
  };

  commonRustBuildEnv = ''
    export ORT_LIB_LOCATION=${onnxruntime}/lib
    export CARGO_PROFILE_RELEASE_LTO=off
    export CARGO_PROFILE_RELEASE_CODEGEN_UNITS=256
  '';

  commonRustBuildEnvWithLinker =
    commonRustBuildEnv
    + lib.optionalString stdenv.isLinux ''
      if [ -n "''${RUSTFLAGS:-}" ]; then
        export RUSTFLAGS="$RUSTFLAGS -C link-arg=-fuse-ld=mold"
      else
        export RUSTFLAGS="-C link-arg=-fuse-ld=mold"
      fi
    '';

  commonBuildEnv = ''
    export SPACEBOT_SKIP_FRONTEND_BUILD=1
    mkdir -p interface/dist
    cp -r ${frontend}/* interface/dist/
  '';

  commonBuildEnvWithLinker = commonRustBuildEnvWithLinker + commonBuildEnv;

  dummyRustSource = pkgs.writeText "dummy.rs" ''
    fn main() {}
  '';

  spacebotCargoArtifacts = craneLib.buildDepsOnly (commonArgs
    // {
      cargoExtraArgs = "--bin spacebot";
      doCheck = false;
      cargoCheckCommand = "true";
      src = craneLib.mkDummySrc {
        src = cargoSrc;
        dummyrs = dummyRustSource;
        dummyBuildrs = "build.rs";
        extraDummyScript = ''
          cp ${dummyRustSource} $out/build.rs
        '';
      };
      preBuild = commonRustBuildEnvWithLinker;
    });

  spacebotTestsCargoArtifacts = craneLib.buildDepsOnly (commonArgs
    // {
      doCheck = false;
      cargoCheckCommand = "true";
      src = craneLib.mkDummySrc {
        src = cargoSrc;
        dummyrs = dummyRustSource;
        dummyBuildrs = "build.rs";
        extraDummyScript = ''
          cp ${dummyRustSource} $out/build.rs
        '';
      };
      preBuild = commonRustBuildEnvWithLinker;
    });

  spacebot = craneLib.buildPackage (commonArgs
    // {
      cargoArtifacts = spacebotCargoArtifacts;
      doCheck = false;
      cargoExtraArgs = "--bin spacebot";

      preBuild = commonBuildEnvWithLinker;

      postInstall = ''
        mkdir -p $out/share/spacebot
        cp -r ${runtimeAssetsSrc}/prompts $out/share/spacebot/
        cp -r ${runtimeAssetsSrc}/migrations $out/share/spacebot/
        chmod -R u+w $out/share/spacebot
      '';

      meta = with lib; {
        description = "An AI agent for teams, communities, and multi-user environments";
        homepage = "https://spacebot.sh";
        license = {
          shortName = "FSL-1.1-ALv2";
          fullName = "Functional Source License, Version 1.1, ALv2 Future License";
          url = "https://fsl.software/";
          free = true;
          redistributable = true;
        };
        platforms = platforms.linux ++ platforms.darwin;
        mainProgram = "spacebot";
      };
    });

  spacebot-tests = craneLib.cargoTest (commonArgs
    // {
      cargoArtifacts = spacebotTestsCargoArtifacts;

      # Skip tests that require ONNX model file and known flaky suites in Nix builds
      cargoTestExtraArgs = "-- --skip memory::search::tests --skip memory::store::tests --skip config::tests::test_llm_provider_tables_parse_with_env_and_lowercase_keys";

      preBuild = commonBuildEnvWithLinker;
    });

  spacebot-full = pkgs.symlinkJoin {
    name = "spacebot-full";
    paths = [spacebot];

    buildInputs = [pkgs.makeWrapper];

    postBuild = ''
      wrapProgram $out/bin/spacebot \
        --set CHROME_PATH "${pkgs.chromium}/bin/chromium" \
        --set CHROME_FLAGS "--no-sandbox --disable-dev-shm-usage --disable-gpu" \
        --set ORT_LIB_LOCATION "${onnxruntime}/lib"
    '';

    meta =
      spacebot.meta
      // {
        description = spacebot.meta.description + " (with browser support)";
      };
  };
in {
  inherit spacebot spacebot-full spacebot-tests;
}

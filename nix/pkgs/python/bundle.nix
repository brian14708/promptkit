{
  stdenv,
  wasipkgs,
  fetchPypi,
  nukeReferences,
}:
let
  inherit (wasipkgs) python;
  inherit (python.host) pythonVersion;
  pythonSitePackages = "lib/python${pythonVersion}/site-packages";

  bundlePackages =
    (builtins.map (pkg: "${pkg}/${pythonSitePackages}") (
      with python.host.pkgs;
      [
        setuptools
        typing-extensions
        annotated-types
        typing-inspection
        xmltodict
        pydantic
        idna
      ]
    ))
    ++ (builtins.map toString [
      (fetchPypi {
        pname = "httpx";
        version = "0.28.1";
        format = "wheel";
        python = "py3";
        dist = "py3";
        hash = "sha256-2Qn8zMEQ+Mf6+BTKgqmk2Ba8Wm2/6iXWWR1phbi6Wa0=";
      })
      (fetchPypi {
        pname = "duron";
        version = "0.0.3";
        format = "wheel";
        python = "py3";
        dist = "py3";
        hash = "sha256-6clLYJzGSNPzVZESBaAt1lYe16Q6zNL+buMvgobKKo4=";
      })
    ]);

  outPackages =
    (with wasipkgs.pythonPackages; [
      numpy
      pillow
      pydantic-core
    ])
    ++ (with python.host.pkgs; [
      tzdata
    ]);

  isolaPy = ../../../crates/python-runtime/python;
in
stdenv.mkDerivation {
  pname = "wasi-python-bundle";
  inherit (python) version;
  dontUnpack = true;
  dontStrip = true;
  PYTHONHASHSEED = "0";

  nativeBuildInputs = [
    python.host
    nukeReferences
  ];
  buildPhase = ''
    runHook preBuild

    # Run bundle.py similar to xtask/src/main.rs
    mkdir -p $TMPDIR/bundle

    cp -r ${isolaPy} $TMPDIR/py
    chmod -R +w $TMPDIR/py

    ${python.host}/bin/python3 ${./bundle.py} $TMPDIR/bundle \
      ${builtins.concatStringsSep " " bundlePackages} \
      $TMPDIR/py

    runHook postBuild
  '';

  installPhase =
    let
      outPackagesList = builtins.concatStringsSep " " (builtins.map toString outPackages);
    in
    ''
      runHook preInstall

      mkdir -p $out/${pythonSitePackages} $out/lib/python${pythonVersion}/lib-dynload
      for pkg in ${outPackagesList}; do
        cp --no-preserve=mode -rL $pkg/${pythonSitePackages}/* $out/${pythonSitePackages}/
      done
      touch $out/lib/python${pythonVersion}/lib-dynload/.empty

      cp $TMPDIR/bundle.zip ${python}/lib/python*.zip $out/lib/

      find $out/ -type f -name "*.so" -exec truncate -s 0 {} \;
      find $out/ -type d -name "__pycache__" -exec rm -rf {} +
      find $out/ -type d -name "*-info" -exec rm -rf {} +

      # compileall uses all available cores when passed 0; this also handles
      # builders that do not export NIX_BUILD_CORES.
      ${python.host}/bin/python3 -m compileall -q -j "''${NIX_BUILD_CORES:-0}" $out/
      find $out/ -type f -exec nuke-refs '{}' +

      runHook postInstall
    '';
}

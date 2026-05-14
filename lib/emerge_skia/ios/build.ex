defmodule EmergeSkia.Ios.Build do
  @moduledoc """
  Builds the OTP runtime + emerge_skia staticlib for iOS (arm64).

  Follows the Nerves convention of declarative configuration and Elixir
  orchestration rather than raw shell scripts.
  """

  defstruct [
    :otp_tag,
    :otp_source,
    :openssl_version,
    :ios_min_ver,
    :build_dir,
    :output_dir,
    :emerge_root
  ]

  @defaults %{
    otp_tag: "OTP-28.3",
    otp_source: "https://github.com/erlang/otp.git",
    openssl_version: "3.4.1",
    ios_min_ver: "15.0",
    emerge_root: nil,
    build_dir: nil,
    output_dir: nil
  }

  @doc """
  Returns the default configuration, resolved relative to the project root.

  Path defaults:

    build_dir  → <project>/_build/ios_otp
    output_dir → <project>/_build/ios_otp_output

  Override any field via the keyword list, or via env vars:

    OTP_TAG, IOS_MIN_VER, IOS_BUILD_DIR, IOS_OUTPUT_DIR
  """
  def default_config(overrides \\ []) do
    emerge_root = overrides[:emerge_root] || EmergeSkia.Ios.Dep.emerge_path!()

    build_dir =
      overrides[:build_dir] || env_or_default("IOS_BUILD_DIR", Path.join(File.cwd!(), "_build/ios_otp"))

    output_dir =
      overrides[:output_dir] ||
        env_or_default("IOS_OUTPUT_DIR", Path.join(File.cwd!(), "_build/ios_otp_output"))

    %__MODULE__{
      otp_tag: overrides[:otp_tag] || env_or_default("OTP_TAG", @defaults.otp_tag),
      otp_source: overrides[:otp_source] || @defaults.otp_source,
      openssl_version: overrides[:openssl_version] || @defaults.openssl_version,
      ios_min_ver: overrides[:ios_min_ver] || env_or_default("IOS_MIN_VER", @defaults.ios_min_ver),
      emerge_root: emerge_root,
      build_dir: build_dir,
      output_dir: output_dir
    }
  end

  @doc """
  Runs the full iOS build pipeline.

  Steps:

    1. build_rust_staticlib/1  — cargo rustc --crate-type staticlib
    2. clone_or_update_otp/1   — git clone/fetch OTP source
    3. build_otp/1             — otp_build configure --without-ssl + boot + make release
    4. create_xcframework/1    — package liberlang.a into xcframework
    5. bundle_elixir/1         — copy host Elixir libs into OTP release

  Note: OpenSSL is NOT built for iOS.  Following the mob framework approach,
  OTP is built with `--without-ssl` and crypto is linked at app-build time.
  This avoids the public_key/ssl build entirely (id-slh-dsa-shake-256f issue).
  """
  def run(config \\ default_config()) do
    assert_xcode_available!()

    info("iOS Build", "OTP #{config.otp_tag}, iOS #{config.ios_min_ver}+, arm64")
    info("Output", config.output_dir)

    with :ok <- build_rust_staticlib(config),
         :ok <- clone_or_update_otp(config),
         :ok <- build_otp(config),
         :ok <- create_xcframework(config),
         :ok <- bundle_elixir(config) do
      info("Done", """
      OTP release:  #{Path.join(config.output_dir, "otp_release")}
      xcframework:  #{Path.join(config.output_dir, "liberlang.xcframework")}
      """)
      :ok
    end
  end

  # ---------------------------------------------------------------------------
  # Steps
  # ---------------------------------------------------------------------------

  def build_rust_staticlib(config) do
    step("Build emerge_skia staticlib", "aarch64-apple-ios")

    native_dir = Path.join(config.emerge_root, "native/emerge_skia")

    # Skia's GN bootstrap needs SSL certs; macOS Python may not have them configured
    ssl_cert = System.get_env("SSL_CERT_FILE") || "/etc/ssl/cert.pem"

    cmd!("cargo", [
      "rustc",
      "--no-default-features",
      "--features",
      "ios",
      "--target",
      "aarch64-apple-ios",
      "--release",
      "--crate-type",
      "staticlib"
    ],
      cd: native_dir,
      description: "cargo rustc --crate-type staticlib for iOS",
      env: [{"SSL_CERT_FILE", ssl_cert}]
    )

    lib_path = Path.join(native_dir,
      "target/aarch64-apple-ios/release/libemerge_skia.a")

    if !File.exists?(lib_path) do
      error!("Static library not found at #{lib_path}")
    end

    info("Size", File.stat!(lib_path).size |> format_bytes())

    unless symbol_present?(lib_path, "_emerge_skia_nif_init") do
      error!("Missing symbol _emerge_skia_nif_init in #{lib_path}")
    end

    info("Symbol", "_emerge_skia_nif_init confirmed")
    :ok
  end

  def clone_or_update_otp(config) do
    step("Clone/update OTP source", config.otp_tag)

    otp_src = Path.join(config.build_dir, "otp_src")

    if File.dir?(otp_src) do
      info("Updating", "existing clone at #{otp_src}")
      cmd!("git", ["fetch", "--tags", "--depth", "1"],
        cd: otp_src, description: "git fetch tags")
      cmd!("git", ["checkout", config.otp_tag],
        cd: otp_src, description: "git checkout #{config.otp_tag}")
      # Remove configure-generated files and build artifacts from previous runs.
      # Generated Makefiles may have hardcoded bad values (e.g. empty TTF_DIR)
      # if a prior configure had wrong variables. A fresh start is the only
      # reliable fix.
      cmd!("git", ["clean", "-fdx"],
        cd: otp_src, description: "git clean -fdx (remove generated files)")
    else
      File.mkdir_p!(config.build_dir)
      cmd!("git", ["clone", "--depth", "1", "--branch", config.otp_tag,
        config.otp_source, otp_src],
        description: "git clone OTP #{config.otp_tag}")
    end

    :ok
  end

  def build_openssl(config) do
    step("Build OpenSSL for iOS", config.openssl_version)

    openssl_install = Path.join(config.build_dir, "openssl")
    openssl_src = Path.join(config.build_dir, "openssl_src")

    if File.dir?(openssl_install) do
      info("Skip", "OpenSSL already installed at #{openssl_install}")
    else
      unless File.dir?(openssl_src) do
        info("Download", "OpenSSL #{config.openssl_version}")
        File.mkdir_p!(config.build_dir)

        tarball = "openssl-#{config.openssl_version}.tar.gz"
        url = "https://www.openssl.org/source/#{tarball}"
        cmd!("curl", ["-#", "-LO", url], cd: config.build_dir,
          description: "download OpenSSL #{config.openssl_version}")

        cmd!("tar", ["-xzf", tarball], cd: config.build_dir,
          description: "extract OpenSSL tarball")

        extracted = Path.join(config.build_dir, "openssl-#{config.openssl_version}")
        File.rename!(extracted, openssl_src)
      end

      cmd!("/bin/sh", ["-c", """
        cd "#{openssl_src}" && \
        ./Configure \
          ios64-xcrun \
          no-shared no-async no-dso no-engine \
          --prefix="#{openssl_install}" && \
        make clean && \
        make depend && \
        make -j#{cpu_count()} && \
        make install_sw install_ssldirs
      """],
        description: "configure + build OpenSSL for iOS"
      )

      info("OpenSSL installed", openssl_install)
    end

    :ok
  end

  def build_otp(config) do
    step("Configure and build OTP", "otp_build + --without-ssl (mob approach)")

    otp_src = Path.join(config.build_dir, "otp_src")

    # OTP's xcomp conf uses `ld` directly which doesn't understand compiler
    # flags like -fstack-protector-strong.  Switch to `cc` as the linker driver.
    xcomp_conf = Path.join(otp_src, "xcomp/erl-xcomp-arm64-ios.conf")
    if File.exists?(xcomp_conf) do
      content = File.read!(xcomp_conf)
      if String.contains?(content, ~s(LD="xcrun -sdk $XCOMP_SDK ld $XCOMP_ARCH")) do
        content = String.replace(content,
          ~s(LD="xcrun -sdk $XCOMP_SDK ld $XCOMP_ARCH"),
          ~s(LD="xcrun -sdk $XCOMP_SDK cc $XCOMP_ARCH"))
        File.write!(xcomp_conf, content)
        info("Patched", "xcomp conf: ld → cc as linker driver")
      end
    end

    # Follow mob's approach: --without-ssl for iOS, otp_build configure/boot,
    # then make release.  Static NIFs (emerge_skia) are linked at app-build
    # time, not OTP-build time.  This avoids the public_key/ssl build entirely
    # which sidesteps the id-slh-dsa-shake-256f macro issue.
    cmd!("/bin/sh", ["-c", "./otp_build configure --xcomp-conf=./xcomp/erl-xcomp-arm64-ios.conf --without-ssl"],
      cd: otp_src,
      description: "otp_build configure --without-ssl"
    )

    cmd!("/bin/sh", ["-c", "./otp_build boot"],
      cd: otp_src,
      description: "otp_build boot (build bootstrap)"
    )

    release_root = Path.join(config.output_dir, "otp_release")
    File.rm_rf!(release_root)

    cmd!("/bin/sh", ["-c", "make release RELEASE_ROOT=#{release_root} RELEASE_LIBBEAM=yes"],
      cd: otp_src,
      description: "make release RELEASE_ROOT="
    )

    # Remove inet_gethost — it would be spawned via fork() on iOS, which
    # the sandbox forbids.
    erts_bins = Path.wildcard("#{release_root}/**/erts-*/bin/inet_gethost")
    for bin <- erts_bins, do: File.rm(bin)

    info("OTP release", release_root)
    :ok
  end

  def create_xcframework(config) do
    step("Package OTP release and xcframework", config.output_dir)

    otp_src = Path.join(config.build_dir, "otp_src")
    release_dir = Path.join(config.output_dir, "otp_release")

    # Release was already installed by build_otp via `make release RELEASE_ROOT=`
    unless File.dir?(release_dir) do
      error!("OTP release directory not found at #{release_dir}")
    end

    # Ensure the replacement inet_gethost_native.beam is up-to-date
    ensure_replacement_beam(config)

    # Find the ERTS static library (libbeam.a)
    erts_libs = Path.wildcard("#{release_dir}/**/libbeam.a")

    erts_lib =
      case erts_libs do
        [lib | _] -> lib
        [] -> error!("libbeam.a not found in installed OTP release")
      end

    info("ERTS lib", erts_lib)

    # Gather all ERTS-dependent static libraries from the build tree.
    # libbeam.a references symbols from these, and they must be merged
    # into a single archive so iOS apps can force-load a single library.
    target = "aarch64-apple-ios"
    variant = "opt"

    dep_libs = [
      erts_lib,
      Path.join(otp_src, "erts/lib/internal/#{target}/liberts_internal_r.a"),
      Path.join(otp_src, "erts/lib/internal/#{target}/libethread.a"),
      Path.join(otp_src, "erts/emulator/pcre/obj/#{target}/#{variant}/libepcre.a"),
      Path.join(otp_src, "erts/emulator/zstd/obj/#{target}/#{variant}/libzstd.a"),
      Path.join(otp_src, "erts/emulator/openssl/obj/#{target}/#{variant}/micro-openssl.a"),
      Path.join(otp_src, "erts/emulator/ryu/obj/#{target}/#{variant}/libryu.a")
    ]

    existing_deps = Enum.filter(dep_libs, &File.exists?/1)

    if length(existing_deps) < length(dep_libs) do
      missing = dep_libs -- existing_deps
      warn("xcframework",
        "Some dependent libraries not found (will be excluded): #{Enum.join(missing, ", ")}")
    end

    info("Merging", "#{length(existing_deps)} archives into combined liberlang.a")

    combined = Path.join(config.output_dir, "liberlang.a")

    # Use macOS libtool to merge all .a files into one combined archive
    cmd!("libtool", ["-static", "-o", combined] ++ existing_deps,
      description: "libtool -static -o liberlang.a (merge #{length(existing_deps)} archives)"
    )

    info("Combined size", combined |> File.stat!() |> Map.get(:size) |> format_bytes())

    xcfw = Path.join(config.output_dir, "liberlang.xcframework")
    File.rm_rf!(xcfw)

    # xcodebuild -create-xcframework may fail if Xcode CLI isn't fully configured;
    # fall back to copying the .a directly.
    case System.cmd("xcodebuild", [
      "-create-xcframework",
      "-library", combined,
      "-output", xcfw
    ],
      cd: config.output_dir,
      stderr_to_stdout: true
    ) do
      {_, 0} ->
        info("xcframework", xcfw)
        :ok

      _ ->
        warn("xcframework", "xcodebuild failed; copying .a directly")
        fallback = Path.join(xcfw, "ios-arm64")
        File.mkdir_p!(fallback)
        File.cp!(combined, Path.join(fallback, "libbeam.a"))
        :ok
    end
  end

  def bundle_elixir(config) do
    step("Bundle Elixir runtime", "host Elixir -> OTP release")

    release_dir = Path.join(config.output_dir, "otp_release")
    release_lib = Path.join(release_dir, "lib")

    beam_path = :code.where_is_file('Elixir.Kernel.beam')

    if beam_path == :non_existing do
      warn("Elixir", "Elixir.Kernel.beam not on code path — skipping Elixir bundling")
      :ok
    else
      # Derive the parent lib/ directory that holds all Elixir apps
      # beam_path -> .../elixir/ebin/Elixir.Kernel.beam
      #           -> .../elixir/ebin
      #           -> .../elixir
      #           -> .../lib
      src_lib = beam_path |> Path.dirname() |> Path.dirname() |> Path.dirname()

      apps = File.ls!(src_lib) |> Enum.filter(&File.dir?(Path.join(src_lib, &1)))

      for app <- apps do
        src = Path.join(src_lib, app)
        dest = Path.join(release_lib, app)
        if !File.exists?(dest) do
          File.cp_r!(src, dest)
        end
      end

      info("Elixir bundled", "#{length(apps)} apps (#{Enum.join(apps, ", ")})")
      :ok
    end
  end

  # ---------------------------------------------------------------------------
  # Helpers
  # ---------------------------------------------------------------------------

  defp sdk_path! do
    {result, 0} = System.cmd("xcrun", ["--sdk", "iphoneos", "--show-sdk-path"])
    String.trim(result)
  end

  defp clang_path! do
    {result, 0} = System.cmd("xcrun", ["--sdk", "iphoneos", "-f", "clang"])
    String.trim(result)
  end

  defp clangpp_path! do
    {result, 0} = System.cmd("xcrun", ["--sdk", "iphoneos", "-f", "clang++"])
    String.trim(result)
  end

  defp cpu_count do
    {result, 0} = System.cmd("sysctl", ["-n", "hw.logicalcpu"])
    String.trim(result)
  end

  defp symbol_present?(lib_path, symbol) do
    # nm + grep -q avoids loading the full (56 MB) output into memory
    System.cmd("sh", ["-c", "nm \"#{lib_path}\" 2>/dev/null | grep -q \"#{symbol}\""])
    |> elem(1) == 0
  end

  defp format_bytes(bytes) when bytes < 1024, do: "#{bytes} B"
  defp format_bytes(bytes) when bytes < 1024 * 1024, do: "#{div(bytes, 1024)} KB"
  defp format_bytes(bytes), do: "#{Float.round(bytes / 1024 / 1024, 1)} MB"

  defp assert_xcode_available! do
    System.find_executable("xcodebuild") ||
      error!("xcodebuild not found. Install Xcode 16+ and the command-line tools.")
    System.find_executable("xcrun") ||
      error!("xcrun not found. Install Xcode command-line tools.")
    System.find_executable("cargo") ||
      error!("cargo not found. Install the Rust toolchain.")
  end

  # ---------------------------------------------------------------------------
  # Logging
  # ---------------------------------------------------------------------------

  defp step(label, detail) do
    IO.puts("")
    IO.puts("==> #{label}  (#{detail})")
  end

  defp info(label, message) do
    IO.puts("  #{label}: #{message}")
  end

  defp warn(label, message) do
    IO.puts(:stderr, "  [!] #{label}: #{message}")
  end

  defp error!(message) do
    IO.puts(:stderr, "")
    IO.puts(:stderr, "ERROR: #{message}")
    IO.puts(:stderr, "")
    exit({:shutdown, 1})
  end

  defp env_or_default(key, default) do
    System.get_env(key) || default
  end

  defp cmd!(program, args, opts \\ []) do
    description = Keyword.get(opts, :description, "#{program} #{Enum.join(args, " ")}")
    cd = Keyword.get(opts, :cd)
    extra_env = Keyword.get(opts, :env, [])

    info("Run", description)

    system_opts = [
      into: IO.stream(:stdio, :line),
      stderr_to_stdout: true
    ]

    system_opts =
      if cd, do: Keyword.put(system_opts, :cd, cd), else: system_opts

    system_opts =
      if extra_env != [],
        do: Keyword.put(system_opts, :env, merged_env(extra_env)),
        else: system_opts

    case System.cmd(program, args, system_opts) do
      {_output, 0} ->
        :ok

      {_output, code} ->
        error!("""
        Command failed (exit #{code}): #{description}

        See output above for details.
        """)
    end
  end

  defp merged_env(extra) do
    # :env replaces the inherited environment, so we must include parent vars
    parent = Enum.map(System.get_env(), fn {k, v} -> {k, v} end)
    extra ++ parent
  end

  @doc """
  Ensures the replacement `inet_gethost_native.beam` is present and up-to-date
  in the OTP release directory.

  Compiles the source from `support/ios_template/inet_gethost_native.erl` using
  the release's own `erlc`, then compares a SHA-256 hash against a cached manifest.
  The beam is only copied if the hash differs, avoiding unnecessary writes.

  Safe to call on every build — no-ops gracefully if the release directory or
  compiler aren't available yet.
  """
  def ensure_replacement_beam(config) do
    release_dir = Path.join(config.output_dir, "otp_release")

    kernel_ebin = Path.wildcard("#{release_dir}/**/kernel-*/ebin") |> List.first()
    erts_bin = Path.wildcard("#{release_dir}/**/erts-*/bin") |> List.first()

    erlc_src = Path.join(config.emerge_root, "support/ios_template/inet_gethost_native.erl")

    if kernel_ebin && erts_bin && File.exists?(erlc_src) do
      erlc = Path.join(erts_bin, "erlc")
      kernel_src = Path.join(Path.dirname(kernel_ebin), "src")
      temp_dir = Path.join(config.build_dir, ".inet_gethost_native_build")
      File.mkdir_p!(temp_dir)
      temp_beam = Path.join(temp_dir, "inet_gethost_native.beam")
      manifest = Path.join(release_dir, ".inet_gethost_native_beam_hash")

      case System.cmd(erlc, [
        "+debug_info",
        "-o", temp_dir,
        "-I", kernel_src,
        "-pa", kernel_ebin,
        erlc_src
      ], stderr_to_stdout: true) do
        {_output, 0} ->
          new_hash = hash_file(temp_beam)
          current_hash =
            if File.exists?(manifest),
              do: File.read!(manifest) |> String.trim(),
              else: ""

          if new_hash != current_hash do
            File.cp!(temp_beam, Path.join(kernel_ebin, "inet_gethost_native.beam"))
            File.write!(manifest, new_hash)
            info("inet_gethost_native", "updated (#{String.slice(new_hash, 0, 12)}...)")
          end

          File.rm_rf!(temp_dir)

        {output, _} ->
          warn("erlc", "failed to compile inet_gethost_native.erl: #{output}")
          File.rm_rf!(temp_dir)
      end
    end

    :ok
  end

  defp hash_file(path) do
    :crypto.hash(:sha256, File.read!(path)) |> Base.encode16(case: :lower)
  end
end

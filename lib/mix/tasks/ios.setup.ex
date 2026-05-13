defmodule Mix.Tasks.Ios.Setup do
  @shortdoc "One-time iOS setup: build OTP + Rust NIF for iOS"
  @moduledoc """
  One-time setup for iOS development.

  Builds the OTP runtime and emerge_skia Rust NIF for iOS arm64, caches
  the artifacts at `~/.cache/emerge/ios/`, and generates the Xcode project.

  ## Usage

      mix ios.setup                              # auto-detect everything
      mix ios.setup --otp-tag=OTP-28.3           # custom OTP version
      mix ios.setup --team=ABC123DEFG            # development team for signing

  This is a one-time operation (takes 5-30 minutes depending on hardware).
  After setup, use `mix ios.run` for iterative development.
  """

  use Mix.Task

  @impl Mix.Task
  def run(args) do
    {parsed, _, _} =
      OptionParser.parse(args, strict: [
        otp_tag: :string,
        team: :string
      ])

    emerge_dep = emerge_dep_path!()
    cache_dir = emerge_cache_ios_dir()
    File.mkdir_p!(cache_dir)

    # Check if already cached
    xcframework = Path.join(cache_dir, "liberlang.xcframework")
    libemerge = Path.join(cache_dir, "libemerge_skia.a")

    if File.dir?(xcframework) && File.exists?(libemerge) do
      Mix.shell().info("iOS artifacts already cached at #{cache_dir}")
    else
      Mix.shell().info("Building iOS artifacts (this may take a while)...")

      # Build emerge_skia Rust staticlib if not cached
      unless File.exists?(libemerge) do
        build_rust_staticlib(emerge_dep, cache_dir)
      end

      # Build OTP for iOS if not cached
      unless File.dir?(xcframework) do
        build_otp(emerge_dep, cache_dir, parsed[:otp_tag])
      end
    end

    # Always ensure the replacement inet_gethost_native.beam is up-to-date,
    # even when the cache was populated before the beam was added.
    config = EmergeSkia.Ios.Build.default_config(
      emerge_root: emerge_dep,
      build_dir: Path.join(emerge_cache_root(), "otp"),
      output_dir: cache_dir
    )

    EmergeSkia.Ios.Build.ensure_replacement_beam(config)

    # Now generate the Xcode project
    gen_args =
      if parsed[:team], do: ["--team", parsed[:team]], else: []

    Mix.Task.run("ios.gen", gen_args)

    output_dir = Path.join(File.cwd!(), "ios")
    xcodeproj = hd(Path.wildcard(Path.join(output_dir, "*.xcodeproj")))

    Mix.shell().info([
      "\n",
      IO.ANSI.green(),
      "iOS setup complete!",
      IO.ANSI.reset(),
      "\n\nOpen in Xcode:\n  open #{xcodeproj}",
      "\n\nOr build and deploy with:\n  mix ios.run\n"
    ])
  end

  defp build_rust_staticlib(emerge_dep, cache_dir) do
    Mix.shell().info("Building emerge_skia Rust staticlib for aarch64-apple-ios...")

    native_dir = Path.join(emerge_dep, "native/emerge_skia")

    unless File.dir?(native_dir) do
      Mix.raise("Rust NIF source not found at #{native_dir}")
    end

    # Build
    case System.cmd("cargo", [
      "rustc",
      "--no-default-features",
      "--features", "ios",
      "--target", "aarch64-apple-ios",
      "--release",
      "--crate-type", "staticlib"
    ], cd: native_dir, stderr_to_stdout: true) do
      {_output, 0} ->
        :ok
      {output, code} ->
        Mix.raise("cargo build failed (exit #{code}):\n#{output}")
    end

    # Copy result to cache
    built = Path.join(native_dir,
      "target/aarch64-apple-ios/release/libemerge_skia.a")
    unless File.exists?(built) do
      Mix.raise("Rust staticlib not found at #{built} after build")
    end

    File.cp!(built, Path.join(cache_dir, "libemerge_skia.a"))
    Mix.shell().info("  cached libemerge_skia.a")
  end

  defp build_otp(emerge_dep, cache_dir, otp_tag) do
    Mix.shell().info("Building OTP for iOS (long step — 10-30 min)...")

    cache_root = emerge_cache_root()

    # Configure IOSBuild with output to cache, build artifacts in ~/.cache/emerge/
    config = EmergeSkia.Ios.Build.default_config(
      emerge_root: emerge_dep,
      build_dir: Path.join(cache_root, "otp"),
      output_dir: cache_dir
    )

    # Override OTP tag if specified
    config = if otp_tag, do: %{config | otp_tag: otp_tag}, else: config

    case EmergeSkia.Ios.Build.run(config) do
      :ok ->
        Mix.shell().info("  OTP build complete, artifacts cached")

      {:error, reason} ->
        Mix.raise("OTP build failed: #{reason}")
    end
  end

  defp emerge_cache_root do
    EmergeSkia.Ios.Dep.cache_root()
  end

  defp emerge_dep_path! do
    EmergeSkia.Ios.Dep.emerge_path!()
  end

  defp emerge_cache_ios_dir do
    EmergeSkia.Ios.Dep.cache_ios_dir()
  end
end

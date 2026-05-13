defmodule Mix.Tasks.Ios.Run do
  @shortdoc "Build and deploy the iOS app to a connected device"
  @moduledoc """
  Builds and deploys the iOS app to a connected device using Xcode CLI.

  ## Usage

      mix ios.run                          # build, deploy, and launch on device
      mix ios.run --device=DEVICE_UDID     # deploy to specific device
      mix ios.run --no-deploy              # build only (no deployment)
      mix ios.run --configuration=Release  # release build
      mix ios.run --no-launch              # deploy but don't launch

  ## Prerequisites

    * Run `mix ios.setup` once first
    * A connected iOS device with Developer Mode enabled
    * The device must be trusted by this Mac
  """

  use Mix.Task

  @impl Mix.Task
  def run(args) do
    {parsed, _, _} =
      OptionParser.parse(args, strict: [
        device: :string,
        no_deploy: :boolean,
        no_launch: :boolean,
        no_rebuild: :boolean,
        configuration: :string
      ])

    config = parsed[:configuration] || "Debug"
    should_deploy = !parsed[:no_deploy]
    should_launch = !parsed[:no_launch]
    should_rebuild = !parsed[:no_rebuild]
    device_udid = parsed[:device]

    # Step 1: Rebuild Rust NIF for iOS (avoid stale cache)
    if should_rebuild do
      rebuild_nif!()
    end

    # Step 2: Generate/update Xcode project (compiles + bundles beams)
    Mix.shell().info("==> Generating Xcode project...")
    Mix.Task.run("ios.gen", [])

    # Step 2: Find the Xcode project
    ios_dir = Path.join(File.cwd!(), "ios")
    projects = Path.wildcard(Path.join(ios_dir, "*.xcodeproj"))
    project = case projects do
      [p | _] -> p
      [] -> Mix.raise("No Xcode project found in #{ios_dir}. Run `mix ios.gen` first.")
    end

    scheme = project |> Path.basename() |> String.replace(".xcodeproj", "")

    # Step 3: Detect connected device if deploying
    udid = if should_deploy do
      device_udid || detect_device!()
    end

    # Step 4: Build for device or generic (always clean to avoid stale cache)
    dest = if should_deploy do
      "platform=iOS,id=#{udid}"
    else
      "generic/platform=iOS"
    end

    Mix.shell().info("==> Building #{scheme} (#{config}) for #{dest}...")

    build_args = [
      "-project", project,
      "-scheme", scheme,
      "-configuration", config,
      "-destination", dest,
      "-allowProvisioningUpdates",
      "clean", "build"
    ]

    case System.cmd("xcodebuild", build_args, stderr_to_stdout: true) do
      {output, 0} ->
        Mix.shell().info("Build succeeded!")

        if should_deploy do
          deploy_and_launch(udid, scheme, config, should_launch)
        else
          Mix.shell().info("\nDeploy with:\n  mix ios.run")
        end

      {output, code} ->
        Mix.raise("xcodebuild failed (exit #{code}):\n#{output}")
    end
  end

  defp deploy_and_launch(udid, scheme, config, should_launch) do
    app_path = find_app(scheme, config)
    unless app_path do
      Mix.raise("App bundle not found after build")
    end

    bundle_id = read_bundle_id(app_path)
    Mix.shell().info("==> Installing #{bundle_id} on device...")

    case System.cmd("xcrun", ["devicectl", "device", "install", "app", "--device", udid, app_path],
          stderr_to_stdout: true) do
      {output, 0} ->
        Mix.shell().info("Install succeeded!")

        if should_launch do
          launch_app(udid, bundle_id)
        end

      {output, code} ->
        Mix.raise("devicectl install failed (exit #{code}):\n#{output}")
    end
  end

  defp launch_app(udid, bundle_id) do
    Mix.shell().info("==> Launching #{bundle_id} on device... (Ctrl+C to stop)\n")

    args = [
      "devicectl", "device", "process", "launch",
      "--device", udid,
      "--terminate-existing",
      "--console",
      bundle_id
    ]

    System.cmd("xcrun", args, into: IO.stream(:stdio, :line), stderr_to_stdout: true)

    Mix.shell().info("\nApp terminated.")
  end

  defp read_bundle_id(app_path) do
    plist = Path.join(app_path, "Info.plist")
    case System.cmd("/usr/libexec/PlistBuddy", ["-c", "Print CFBundleIdentifier", plist],
          stderr_to_stdout: true) do
      {id, 0} -> String.trim(id)
      {_err, _code} -> nil
    end
  end

  defp find_app(scheme, config) do
    derived = Path.join(System.user_home!(), "Library/Developer/Xcode/DerivedData")
    candidates =
      if File.dir?(derived) do
        Path.wildcard(Path.join(derived, "*/Build/Products/#{config}-iphoneos/*.app"))
        |> Enum.sort_by(&File.stat!(&1).mtime, :desc)
      else
        []
      end

    case candidates do
      [app | _] -> app
      [] -> nil
    end
  end

  defp rebuild_nif! do
    Mix.shell().info("==> Building Rust NIF for iOS (aarch64-apple-ios)...")

    emerge_dir = EmergeSkia.Ios.Dep.emerge_path!()
    native_dir = Path.join(emerge_dir, "native/emerge_skia")

    unless File.dir?(native_dir) do
      Mix.raise("Rust NIF source not found at #{native_dir}")
    end

    ssl_cert = System.get_env("SSL_CERT_FILE") || "/etc/ssl/cert.pem"

    case System.cmd("cargo", [
      "rustc",
      "--no-default-features",
      "--features", "ios",
      "--target", "aarch64-apple-ios",
      "--release",
      "--crate-type", "staticlib"
    ],
      cd: native_dir,
      stderr_to_stdout: true,
      env: [{"SSL_CERT_FILE", ssl_cert}]
    ) do
      {_output, 0} ->
        lib_path = Path.join(native_dir,
          "target/aarch64-apple-ios/release/libemerge_skia.a")

        unless File.exists?(lib_path) do
          Mix.raise("libemerge_skia.a not found after build at #{lib_path}")
        end

        # Update cache so xcodebuild picks it up (and clears stale cached copy)
        cache_dir = EmergeSkia.Ios.Dep.cache_ios_dir()
        File.mkdir_p!(cache_dir)
        File.cp!(lib_path, Path.join(cache_dir, "libemerge_skia.a"))

        Mix.shell().info("  Rust NIF rebuilt and cached at #{cache_dir}")

      {output, code} ->
        Mix.raise("cargo rustc failed (exit #{code}):\n#{output}")
    end
  end

  defp detect_device! do
    Mix.shell().info("==> Detecting connected iOS device...")

    case System.cmd("xcrun", ~w(devicectl list devices), stderr_to_stdout: true) do
      {output, 0} ->
        devices =
          output
          |> String.split("\n")
          |> Enum.filter(&(String.contains?(&1, "connected")))
          |> Enum.map(fn line ->
            parts = String.split(line)
            name = Enum.at(parts, 0)
            udid = Enum.at(parts, 2)
            {name, udid}
          end)

        case devices do
          [{name, udid} | _] ->
            Mix.shell().info("  Found: #{name} (#{udid})")
            udid

          [] ->
            Mix.raise("""
            No connected iOS device found.
            Connect a device via USB and try again.
            """)
        end

      {_output, _code} ->
        case System.cmd("xcrun", ~w(xctrace list devices), stderr_to_stdout: true) do
          {output2, 0} ->
            devices =
              output2
              |> String.split("\n")
              |> Enum.filter(&(String.contains?(&1, "iPhone") or String.contains?(&1, "iPad")))
              |> Enum.filter(&(not String.contains?(&1, "Simulator")))

            case devices do
              [device_line | _] ->
                case Regex.run(~r/([A-F0-9-]{36})/, device_line) do
                  [_, udid] ->
                    Mix.shell().info("  Found: #{String.trim(device_line)}")
                    udid

                  nil ->
                    Mix.raise("""
                    Could not parse device UDID from:
                      #{device_line}
                    """)
                end

              [] ->
                Mix.raise("""
                No connected iOS device found (checked both devicectl and xctrace).
                Connect a device via USB and try again.
                """)
            end

          {_output2, _code2} ->
            Mix.raise("""
            Could not detect connected device.
            Make sure a device is connected and trusted.
            """)
        end
    end
  end
end

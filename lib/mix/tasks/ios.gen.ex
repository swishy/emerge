defmodule Mix.Tasks.Ios.Gen do
  @shortdoc "Generate the iOS Xcode project"

  @moduledoc """
  Generates the iOS Xcode project.

  ## Usage (from an app that depends on emerge)

      mix ios.gen                              # generates ios/APP.xcodeproj
      mix ios.gen --app=MyApp                  # custom app module name
      mix ios.gen --app-module=my_app          # Elixir app module atom
      mix ios.gen --bundle-id=com.example.app  # bundle identifier
      mix ios.gen --team=ABC123DEFG            # development team
      mix ios.gen --out=./ios                  # output directory

  Run `mix ios.setup` first to ensure pre-built OTP + Rust artifacts exist.
  """

  use Mix.Task

  @impl Mix.Task
  def run(args) do
    {parsed, _, _} =
      OptionParser.parse(args, strict: [
        app: :string,
        app_module: :string,
        bundle_id: :string,
        team: :string,
        out: :string
      ])

    run_app(parsed)
  end

  defp run_app(parsed) do
    emerge_dep = emerge_dep_path!()
    output_dir = Path.absname(parsed[:out] || "ios")
    app_name = parsed[:app] || infer_app_name()
    app_module = parsed[:app_module] || infer_otp_app() || app_name
    otp_app = infer_otp_app() || app_name
    bundle_id = parsed[:bundle_id] || "com.#{app_name}.ios"
    team = parsed[:team] || ""

    Mix.Task.run("compile")

    app_src_dir = Path.join(output_dir, app_name)
    frameworks_dir = Path.join(output_dir, "Frameworks")
    File.mkdir_p!(app_src_dir)
    File.mkdir_p!(frameworks_dir)

    template_dir = Path.join(emerge_dep, "support/ios_template")
    copy_template(template_dir, output_dir, app_name, app_module, otp_app)

    cache_dir = Path.join(emerge_cache_dir(), "ios")
    if File.dir?(cache_dir) do
      for artifact <- ["liberlang.xcframework", "liberlang_combined.a", "libemerge_skia.a"] do
        src = Path.join(cache_dir, artifact)
        dst = Path.join(frameworks_dir, artifact)
        if File.exists?(src) do
          File.cp_r!(src, dst)
        end
      end
    end

    fallback = Path.join(emerge_dep, "_build/ios_otp_output")
    if not File.dir?(cache_dir) and File.dir?(fallback) do
      xcfw_src = Path.join(fallback, "liberlang.xcframework")
      xcfw_dst = Path.join(frameworks_dir, "liberlang.xcframework")
      if File.dir?(xcfw_src) and not File.dir?(xcfw_dst) do
        File.cp_r!(xcfw_src, xcfw_dst)
      end
    end

    app_build_output = Path.join(File.cwd!(), "_build/ios_otp_output")
    app_release_src = Path.join(app_build_output, "otp_release")
    fallback_release_src = Path.join(fallback, "otp_release")
    release_dst = Path.join(app_src_dir, "erlang")

    release_src =
      cond do
        File.dir?(app_release_src) -> app_release_src
        File.dir?(fallback_release_src) -> fallback_release_src
        true -> nil
      end

    if release_src != nil and not File.dir?(release_dst) do
      File.cp_r!(release_src, release_dst)
    end

    bundle_app_beams(emerge_dep, app_src_dir)

    tmpl = Path.join(emerge_dep, "support/ios_template/Project.yml.eex")
    project_yml = Path.join(output_dir, "Project.yml")

    emerge_relative = relative_path(output_dir, emerge_dep)

    assigns = [
      app_name: app_name,
      app_module: app_module,
      otp_app: otp_app,
      bundle_id_prefix: String.split(bundle_id, ".") |> Enum.drop(-1) |> Enum.join("."),
      bundle_identifier: bundle_id,
      development_team: team,
      emerge_root: emerge_relative
    ]

    content = EEx.eval_file(tmpl, assigns: assigns)
    File.write!(project_yml, content)

    xcodeproj = Path.join(output_dir, "#{app_name}.xcodeproj")
    if File.dir?(xcodeproj), do: File.rm_rf!(xcodeproj)

    case System.cmd("xcodegen", ["generate"], cd: output_dir, stderr_to_stdout: true) do
      {_output, 0} ->
        Mix.shell().info([
          "\n",
          IO.ANSI.green(),
          "Created #{xcodeproj}",
          IO.ANSI.reset(),
          "\n\nOpen in Xcode:\n  open #{xcodeproj}\n"
        ])

      {output, code} ->
        Mix.raise("xcodegen failed (exit #{code}):\n#{output}")
    end
  end

  defp emerge_dep_path! do
    EmergeSkia.Ios.Dep.emerge_path!()
  end

  defp emerge_cache_dir do
    EmergeSkia.Ios.Dep.cache_root()
  end

  defp infer_app_name do
    mix_file = Path.join(File.cwd!(), "mix.exs")
    if File.exists?(mix_file) do
      content = File.read!(mix_file)
      case Regex.run(~r/defmodule\s+(\w+)\.MixProject\b/, content) do
        [_, mod] -> mod
        nil -> File.cwd!() |> Path.basename()
      end
    else
      File.cwd!() |> Path.basename()
    end
  end

  defp infer_otp_app do
    mix_file = Path.join(File.cwd!(), "mix.exs")
    if File.exists?(mix_file) do
      content = File.read!(mix_file)
      case Regex.run(~r/app:\s*:(\w+)/, content) do
        [_, app] -> String.to_atom(app)
        nil -> nil
      end
    end
  end

  defp copy_template(template_dir, output_dir, app_name, app_module, otp_app) do
    app_dir = Path.join(output_dir, app_name)
    File.mkdir_p!(app_dir)

    for file <- ~w(BeamBridge.c BeamBridge.h EmergeNifInit.c Info.plist) do
      src = Path.join(template_dir, "EmergeIos/#{file}")
      dst = Path.join(app_dir, file)
      if File.exists?(src) do
        File.cp!(src, dst)
      end
    end

    app_delegate_src = Path.join(template_dir, "EmergeIos/AppDelegate.swift.eex")
    app_delegate_dst = Path.join(app_dir, "AppDelegate.swift")
    if File.exists?(app_delegate_src) and not File.exists?(app_delegate_dst) do
      assigns = [app_name: app_name, app_module: app_module, otp_app: otp_app]
      content = EEx.eval_file(app_delegate_src, assigns: assigns)
      File.write!(app_delegate_dst, content)
    end

    storyboard_src = Path.join(template_dir, "EmergeIos/LaunchScreen.storyboard.eex")
    storyboard_dst = Path.join(app_dir, "LaunchScreen.storyboard")
    if File.exists?(storyboard_src) and not File.exists?(storyboard_dst) do
      assigns = [app_name: app_name, app_module: app_module]
      content = EEx.eval_file(storyboard_src, assigns: assigns)
      File.write!(storyboard_dst, content)
    end

    bridging_header = Path.join(app_dir, "#{app_name}-Bridging-Header.h")
    if not File.exists?(bridging_header) do
      File.write!(bridging_header, "#import \"BeamBridge.h\"\n")
    end
  end

  defp bundle_app_beams(emerge_dep, app_src_dir) do
    lib_dir = Path.join(app_src_dir, "erlang/lib")

    app_build_output = Path.join(File.cwd!(), "_build/ios_otp_output")
    emerge_build_output = Path.join(emerge_dep, "_build/ios_otp_output")

    release_lib =
      cond do
        File.dir?(Path.join(app_build_output, "otp_release/lib")) ->
          Path.join(app_build_output, "otp_release/lib")

        File.dir?(Path.join(emerge_build_output, "otp_release/lib")) ->
          Path.join(emerge_build_output, "otp_release/lib")

        File.dir?(Path.join(app_build_output, "otp_release/usr/local/lib/erlang/lib")) ->
          Path.join(app_build_output, "otp_release/usr/local/lib/erlang/lib")

        File.dir?(Path.join(emerge_build_output, "otp_release/usr/local/lib/erlang/lib")) ->
          Path.join(emerge_build_output, "otp_release/usr/local/lib/erlang/lib")

        true ->
          nil
      end

    if release_lib do
      for appdir <- File.ls!(release_lib) do
        src = Path.join(release_lib, appdir)
        dst = Path.join(lib_dir, appdir)
        if not File.dir?(dst) do
          File.cp_r!(src, dst)
        end
      end
    end

    Mix.shell().info("Bundling app beams from #{File.cwd!()}/_build/dev/lib/")
    for dir <- Path.wildcard(Path.join(File.cwd!(), "_build/dev/lib/*/")) do
      appname = Path.basename(dir)
      srcebin = Path.join(dir, "ebin")
      if File.dir?(srcebin) do
        dstdir = Path.join(lib_dir, appname)
        File.mkdir_p!(Path.join(dstdir, "ebin"))
        for beam <- Path.wildcard(Path.join(srcebin, "*.beam")) do
          File.cp!(beam, Path.join(Path.join(dstdir, "ebin"), Path.basename(beam)))
        end
        for appfile <- Path.wildcard(Path.join(srcebin, "*.app")) do
          File.cp!(appfile, Path.join(Path.join(dstdir, "ebin"), Path.basename(appfile)))
        end
        Mix.shell().info("  Bundled #{appname}")
      end
    end

    emerge_lib = Path.join(emerge_dep, "_build/dev/lib/emerge/ebin")
    if File.dir?(emerge_lib) do
      dstdir = Path.join(lib_dir, "emerge")
      File.mkdir_p!(Path.join(dstdir, "ebin"))
      for beam <- Path.wildcard(Path.join(emerge_lib, "*.beam")) do
        File.cp!(beam, Path.join(Path.join(dstdir, "ebin"), Path.basename(beam)))
      end
      for appfile <- Path.wildcard(Path.join(emerge_lib, "*.app")) do
        File.cp!(appfile, Path.join(Path.join(dstdir, "ebin"), Path.basename(appfile)))
      end
    end
  end

  defp relative_path(from, to) do
    from_segments = Path.split(from)
    to_segments = Path.split(to)

    common =
      Enum.zip(from_segments, to_segments)
      |> Enum.take_while(fn {a, b} -> a == b end)
      |> Enum.count()

    up = List.duplicate("..", length(from_segments) - common)
    down = Enum.drop(to_segments, common)
    segments = up ++ down

    if segments == [], do: ".", else: Path.join(segments)
  end
end

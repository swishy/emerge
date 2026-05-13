defmodule EmergeSkia.Ios.Dep do
  @moduledoc false

  @doc """
  Resolves the absolute path to the emerge dependency.

  Handles three cases:
    - **Path dep** – parses `path:` from the project's mix.exs
    - **Hex dep** – returns deps/emerge/ relative to the project root
    - **Inside emerge itself** – returns the current project root
  """
  def emerge_path! do
    mix_file = Path.join(File.cwd!(), "mix.exs")

    if File.exists?(mix_file) do
      content = File.read!(mix_file)

      case Regex.run(~r/{:emerge,\s*path:\s*"([^"]+)"}/, content) do
        [_, path] ->
          Path.absname(path, File.cwd!())

        nil ->
          deps_path = Path.join(File.cwd!(), "deps/emerge")

          if File.dir?(deps_path) do
            deps_path
          else
            if File.dir?(Path.join(File.cwd!(), "native/emerge_skia")) do
              File.cwd!()
            else
              Mix.raise("""
              Cannot find the emerge dependency.
              Expected at deps/emerge/ (after mix deps.get) or declared with
              a path: option in mix.exs:

                {:emerge, path: "../emerge"}
              """)
            end
          end
      end
    else
      Mix.raise("No mix.exs found in #{File.cwd!()}")
    end
  end

  def cache_ios_dir do
    Path.join([System.user_home!(), ".cache", "emerge", "ios"])
  end

  def cache_otp_dir do
    Path.join([System.user_home!(), ".cache", "emerge", "otp"])
  end

  def cache_root do
    Path.join(System.user_home!(), ".cache/emerge")
  end
end

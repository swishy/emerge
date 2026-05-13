defmodule Mix.Tasks.Ios.Build do
  @shortdoc "Build the iOS app: Rust staticlib + OTP with --enable-static-nifs"
  @moduledoc """
  Builds everything needed for the iOS app.

  ## Usage

      mix ios.build                        # default config
      mix ios.build --otp-tag=OTP-28.3    # override OTP version

  ## Configuration

  All options can be passed as `--key=value` flags or set as environment variables:

  | Flag            | Env var         | Default          |
  |-----------------|-----------------|------------------|
  | `--otp-tag`     | `OTP_TAG`       | OTP-28.3         |
  | `--ios-min-ver` | `IOS_MIN_VER`   | 15.0             |
  | `--build-dir`   | `IOS_BUILD_DIR` | _build/ios_otp   |
  | `--output-dir`  | `IOS_OUTPUT_DIR`| _build/ios_otp_output |

  ## Prerequisites

    * Xcode 16+ with iOS SDK command-line tools (`xcode-select --install`)
    * Rust toolchain with `aarch64-apple-ios` target:
        rustup target add aarch64-apple-ios
    * Elixir + Erlang/OTP 28+ on the host (for building the Elixir release)
  """

  use Mix.Task

  @impl Mix.Task
  def run(args) do
    {parsed, _, _} = OptionParser.parse(args, strict: [
      otp_tag: :string,
      otp_source: :string,
      ios_min_ver: :string,
      build_dir: :string,
      output_dir: :string
    ])

    config = EmergeSkia.Ios.Build.default_config(
      otp_tag: parsed[:otp_tag],
      otp_source: parsed[:otp_source],
      ios_min_ver: parsed[:ios_min_ver],
      build_dir: parsed[:build_dir],
      output_dir: parsed[:output_dir]
    )

    EmergeSkia.Ios.Build.run(config)
  end
end

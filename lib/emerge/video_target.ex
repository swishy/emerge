defmodule Emerge.VideoTarget do
  @moduledoc """
  UI-level abstraction for renderer-owned video targets.

  `Emerge.UI.video/2` accepts this struct directly and also normalizes legacy
  `%EmergeSkia.VideoTarget{}` values for compatibility with existing renderer
  code. Most applications still obtain targets from `EmergeSkia.video_target/2`,
  but higher-level Emerge APIs only need the stable fields captured here.
  """

  @enforce_keys [:id, :width, :height, :mode, :ref]
  defstruct [:id, :width, :height, :mode, :ref]

  @type mode :: :prime

  @type t :: %__MODULE__{
          id: String.t(),
          width: pos_integer(),
          height: pos_integer(),
          mode: mode(),
          ref: reference()
        }

  @type compatible_t :: t() | EmergeSkia.VideoTarget.t()

  @doc """
  Build an Emerge-owned video target value.
  """
  @spec new(keyword()) :: t()
  def new(opts) when is_list(opts) do
    opts = Keyword.new(opts)
    id = Keyword.fetch!(opts, :id)
    width = Keyword.fetch!(opts, :width)
    height = Keyword.fetch!(opts, :height)
    mode = Keyword.get(opts, :mode, :prime)
    ref = Keyword.fetch!(opts, :ref)

    if !is_binary(id) do
      raise ArgumentError, "video target id must be a binary"
    end

    if !is_integer(width) or width <= 0 do
      raise ArgumentError, "video target width must be a positive integer"
    end

    if !is_integer(height) or height <= 0 do
      raise ArgumentError, "video target height must be a positive integer"
    end

    if mode != :prime do
      raise ArgumentError, "video target mode must be :prime in v1"
    end

    if !is_reference(ref) do
      raise ArgumentError, "video target ref must be a reference"
    end

    %__MODULE__{id: id, width: width, height: height, mode: mode, ref: ref}
  end

  @doc """
  Convert a legacy `%EmergeSkia.VideoTarget{}` into the Emerge-owned struct.
  """
  @spec from_skia(EmergeSkia.VideoTarget.t()) :: t()
  def from_skia(%EmergeSkia.VideoTarget{} = target) do
    %__MODULE__{
      id: target.id,
      width: target.width,
      height: target.height,
      mode: target.mode,
      ref: target.ref
    }
  end

  @doc """
  Normalize a compatible video target into `%Emerge.VideoTarget{}`.
  """
  @spec normalize!(compatible_t()) :: t()
  def normalize!(%__MODULE__{} = target), do: target
  def normalize!(%EmergeSkia.VideoTarget{} = target), do: from_skia(target)

  def normalize!(other) do
    raise ArgumentError,
          "expected an Emerge.VideoTarget or legacy EmergeSkia.VideoTarget, got: #{inspect(other)}"
  end
end

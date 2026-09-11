defmodule EmergeSkia.VideoTarget do
  @moduledoc """
  Legacy compatibility struct for renderer-owned video targets.

  `EmergeSkia.video_target/2` still returns this struct for backwards
  compatibility. Higher-level Emerge APIs normalize both this legacy value and
  `%Emerge.VideoTarget{}` through `Emerge.VideoTarget`.

  `id` is serialized into the UI tree, while `ref` is passed to native submit APIs.
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
end

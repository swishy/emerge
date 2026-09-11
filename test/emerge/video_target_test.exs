defmodule Emerge.VideoTargetTest do
  use ExUnit.Case, async: true

  alias Emerge.VideoTarget

  test "new/1 builds an Emerge-owned target" do
    ref = make_ref()
    target = VideoTarget.new(id: "preview", width: 640, height: 360, ref: ref)

    assert target == %VideoTarget{
             id: "preview",
             width: 640,
             height: 360,
             mode: :prime,
             ref: ref
           }
  end

  test "normalize!/1 converts legacy EmergeSkia.VideoTarget values" do
    legacy = %EmergeSkia.VideoTarget{
      id: "legacy",
      width: 320,
      height: 180,
      mode: :prime,
      ref: make_ref()
    }

    assert %VideoTarget{id: "legacy", width: 320, height: 180, mode: :prime} =
             VideoTarget.normalize!(legacy)
  end

  test "normalize!/1 rejects unsupported values" do
    assert_raise ArgumentError,
                 ~r/expected an Emerge\.VideoTarget or legacy EmergeSkia\.VideoTarget/,
                 fn ->
                   VideoTarget.normalize!(:bad)
                 end
  end
end

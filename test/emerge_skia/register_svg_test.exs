defmodule EmergeSkia.RegisterSvgTest do
  @moduledoc """
  Runtime inline-SVG pipeline: register SVG bytes under a logical id, then
  render a tree referencing `{:id, id}` to an offscreen PNG without any asset
  files on disk.
  """

  use ExUnit.Case, async: false

  import Emerge.UI
  import Emerge.UI.Size

  @circle ~s(<svg xmlns="http://www.w3.org/2000/svg" width="64" height="64" viewBox="0 0 64 64"><circle cx="32" cy="32" r="28" fill="#00b5c8"/></svg>)

  test "register_svg/2 returns the intrinsic dimensions" do
    assert {:ok, {64, 64}} = EmergeSkia.register_svg("test:circle", @circle)
  end

  test "register_svg/2 reports a parse error for bad markup" do
    assert {:error, reason} = EmergeSkia.register_svg("test:bad", "not svg at all")
    assert is_binary(reason)
  end

  test "a registered inline SVG renders via {:id, id} in the offscreen path" do
    assert {:ok, {64, 64}} = EmergeSkia.register_svg("test:circle2", @circle)

    tree =
      el(
        [width(px(64)), height(px(64))],
        svg([width(px(64)), height(px(64))], {:id, "test:circle2"})
      )

    png = EmergeSkia.render_to_png(tree, otp_app: :emerge, width: 64, height: 64)

    assert <<0x89, ?P, ?N, ?G, 0x0D, 0x0A, 0x1A, 0x0A, _::binary>> = png
    assert byte_size(png) > 100
  end

  test "re-registering the same id replaces the asset" do
    assert {:ok, {64, 64}} = EmergeSkia.register_svg("test:replace", @circle)

    bigger =
      ~s(<svg xmlns="http://www.w3.org/2000/svg" width="128" height="96"><rect width="128" height="96" fill="#7b2cbf"/></svg>)

    assert {:ok, {128, 96}} = EmergeSkia.register_svg("test:replace", bigger)
  end
end

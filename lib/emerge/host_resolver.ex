defmodule Emerge.HostResolver do
  @moduledoc """
  Resolves hostnames via `EmergeSkia.resolve_host/2` (NIF + DNS/mDNS fallback).

  Registered as `:emerge_host_resolver` so that the Erlang kernel module
  `inet_gethost_native` can delegate hostname resolution to it via message
  passing — avoiding the port program spawn that iOS forbids.
  """

  use GenServer

  @name :emerge_host_resolver

  @doc """
  Start the resolver and register it globally.
  """
  @spec start_link(keyword()) :: GenServer.on_start()
  def start_link(opts \\ []) do
    GenServer.start_link(__MODULE__, :ok, Keyword.merge(opts, name: @name))
  end

  @impl true
  def init(:ok) do
    {:ok, %{}}
  end

  @impl true
  def handle_info({from, ref, {:resolve, name, proto_code}}, state) do
    result = do_resolve(name, proto_code)
    send(from, {ref, result})
    {:noreply, state}
  end

  def handle_info({from, ref, {:resolve_by_addr, addr_tuple, proto_code}}, state) do
    result = do_reverse_resolve(addr_tuple, proto_code)
    send(from, {ref, result})
    {:noreply, state}
  end

  defp do_resolve(name, proto_code) do
    try do
      case EmergeSkia.resolve_host(name, family(proto_code)) do
        {:ok, ips} -> {:ok, ips}
        {:error, _} -> {:error, :nxdomain}
      end
    rescue
      _ -> {:error, :nxdomain}
    end
  end

  defp do_reverse_resolve(_addr_tuple, _proto_code) do
    {:error, :nxdomain}
  end

  defp family(1), do: :inet
  defp family(2), do: :inet6
end

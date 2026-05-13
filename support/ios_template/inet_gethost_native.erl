%% Replacement for OTP's inet_gethost_native.erl
%%
%% The OTP original spawns the inet_gethost port program via
%% open_port({spawn_executable, ...}), which iOS forbids and triggers
%% erlang:halt() in open_executable/2 (line 425-426).
%%
%% This replacement delegates hostname resolution to the registered
%% emerge_host_resolver process, which calls the emerge_skia Rust NIF
%% that uses POSIX getaddrinfo (respects system mDNS/Bonjour).
%%
%% No port program needed — no crash.
-module(inet_gethost_native).

-export([gethostbyname/1, gethostbyname/2, gethostbyaddr/1, control/1]).

-include_lib("kernel/include/inet.hrl").

-define(PROTO_IPV4, 1).
-define(PROTO_IPV6, 2).

%% Timeout for the resolver process (60s)
-define(RESOLVE_TIMEOUT_MS, 60000).

%% ===========================================================================
%% Public API (matches OTP's inet_gethost_native.erl)
%% ===========================================================================

gethostbyname(Name) ->
    gethostbyname(Name, inet).

gethostbyname(Name, inet) ->
    case resolve_host(Name, ?PROTO_IPV4) of
        {ok, Addresses} ->
            {ok, #hostent{
                h_name = Name,
                h_aliases = [],
                h_addrtype = inet,
                h_length = 4,
                h_addr_list = Addresses
            }};
        Error -> Error
    end;
gethostbyname(Name, inet6) ->
    case resolve_host(Name, ?PROTO_IPV6) of
        {ok, Addresses} ->
            {ok, #hostent{
                h_name = Name,
                h_aliases = [],
                h_addrtype = inet6,
                h_length = 16,
                h_addr_list = Addresses
            }};
        Error -> Error
    end.

gethostbyaddr(Addr) when tuple_size(Addr) =:= 4 ->
    resolve_addr(Addr, ?PROTO_IPV4);
gethostbyaddr(Addr) when tuple_size(Addr) =:= 8 ->
    resolve_addr(Addr, ?PROTO_IPV6).

control(_) -> ok.

%% ===========================================================================
%% Internal
%% ===========================================================================

resolve_host(Name, ProtoCode) ->
    case erlang:whereis(emerge_host_resolver) of
        undefined ->
            %% Resolver not yet registered — retry briefly, then fail gracefully
            timer:sleep(100),
            retry_resolve(Name, ProtoCode, 4);
        Pid ->
            do_request(Pid, {resolve, Name, ProtoCode})
    end.

retry_resolve(_Name, _ProtoCode, 0) ->
    {error, nxdomain};
retry_resolve(Name, ProtoCode, Retries) ->
    case erlang:whereis(emerge_host_resolver) of
        undefined ->
            timer:sleep(100),
            retry_resolve(Name, ProtoCode, Retries - 1);
        Pid ->
            do_request(Pid, {resolve, Name, ProtoCode})
    end.

resolve_addr(Addr, ProtoCode) ->
    case erlang:whereis(emerge_host_resolver) of
        undefined ->
            {error, nxdomain};
        Pid ->
            do_request(Pid, {resolve_by_addr, Addr, ProtoCode})
    end.

do_request(Pid, Request) ->
    Ref = erlang:monitor(process, Pid),
    Pid ! {self(), Ref, Request},
    receive
        {Ref, Result} ->
            erlang:demonitor(Ref, [flush]),
            Result;
        {'DOWN', Ref, _, _, _} ->
            {error, nxdomain}
    after ?RESOLVE_TIMEOUT_MS ->
        erlang:demonitor(Ref, [flush]),
        {error, timeout}
    end.

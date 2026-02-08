import os from "node:os";
import path from "node:path";
import {
  DEFAULT_ACCOUNT_ID,
  formatPairingApproveHint,
  type ChannelPlugin,
} from "openclaw/plugin-sdk";
import { getMarmotRuntime } from "./runtime.js";
import {
  listMarmotAccountIds,
  resolveDefaultMarmotAccountId,
  resolveMarmotAccount,
  type ResolvedMarmotAccount,
} from "./types.js";
import { MarmotSidecar, resolveAccountStateDir } from "./sidecar.js";

type MarmotSidecarHandle = {
  sidecar: MarmotSidecar;
  pubkey: string;
  npub: string;
};

const activeSidecars = new Map<string, MarmotSidecarHandle>();

function looksLikeGroupIdHex(input: string): boolean {
  return /^[0-9a-f]{64}$/i.test(input.trim());
}

function normalizeGroupId(input: string): string {
  const trimmed = input.trim();
  if (!trimmed) return trimmed;
  return trimmed
    .replace(/^marmot:/i, "")
    .replace(/^group:/i, "")
    .replace(/^marmot:group:/i, "")
    .trim()
    .toLowerCase();
}

function parseReplyExactly(text: string): string | null {
  const m = text.match(/^openclaw:\s*reply exactly\s*\"([^\"]*)\"\s*$/i);
  return m ? m[1] ?? "" : null;
}

function resolveSidecarCmd(cfgCmd?: string | null): string | null {
  const env = process.env.MARMOT_SIDECAR_CMD?.trim();
  if (env) return env;
  const trimmed = String(cfgCmd ?? "").trim();
  return trimmed ? trimmed : null;
}

function resolveSidecarArgs(cfgArgs?: string[] | null): string[] | null {
  const env = process.env.MARMOT_SIDECAR_ARGS?.trim();
  if (env) {
    try {
      const parsed = JSON.parse(env);
      if (Array.isArray(parsed) && parsed.every((x) => typeof x === "string")) {
        return parsed;
      }
    } catch {
      // ignore
    }
  }
  if (Array.isArray(cfgArgs) && cfgArgs.every((x) => typeof x === "string")) {
    return cfgArgs;
  }
  return null;
}

export const marmotPlugin: ChannelPlugin<ResolvedMarmotAccount> = {
  id: "marmot",
  meta: {
    id: "marmot",
    label: "Marmot",
    selectionLabel: "Marmot (Rust)",
    docsPath: "/channels/marmot",
    docsLabel: "marmot",
    blurb: "MLS E2EE groups over Nostr (Rust sidecar).",
    order: 56,
    quickstartAllowFrom: true,
  },
  capabilities: {
    chatTypes: ["group"],
    media: false,
    nativeCommands: false,
  },
  reload: { configPrefixes: ["channels.marmot", "plugins.entries.marmot"] },

  config: {
    listAccountIds: (cfg) => listMarmotAccountIds(cfg),
    resolveAccount: (cfg, accountId) => resolveMarmotAccount({ cfg, accountId }),
    defaultAccountId: (cfg) => resolveDefaultMarmotAccountId(cfg),
    setAccountEnabled: async () => {
      throw new Error("marmot: multi-account enable/disable not implemented yet");
    },
    deleteAccount: async () => {
      throw new Error("marmot: multi-account delete not implemented yet");
    },
    isConfigured: (account) => account.configured,
    describeAccount: (account) => ({
      accountId: account.accountId,
      name: account.name,
      enabled: account.enabled,
      configured: account.configured,
    }),
    resolveAllowFrom: ({ cfg, accountId }) =>
      (resolveMarmotAccount({ cfg, accountId }).config.groupAllowFrom ?? []).map((x) => String(x)),
    formatAllowFrom: ({ allowFrom }) =>
      allowFrom
        .map((x) => String(x).trim().toLowerCase())
        .filter(Boolean),
  },

  // For now: no DMs, but keep the pairing surface stubbed so OpenClaw help output stays consistent.
  pairing: {
    idLabel: "marmotPubkey",
    normalizeAllowEntry: (entry) => entry.replace(/^marmot:/i, "").trim().toLowerCase(),
    notifyApproval: async () => {
      // Not implemented (DMs not implemented yet).
    },
  },
  security: {
    resolveDmPolicy: () => ({
      policy: "pairing",
      allowFrom: [],
      policyPath: "channels.marmot.dmPolicy",
      allowFromPath: "channels.marmot.allowFrom",
      approveHint: formatPairingApproveHint("marmot"),
      normalizeEntry: (raw) => raw.replace(/^marmot:/i, "").trim().toLowerCase(),
    }),
  },

  messaging: {
    normalizeTarget: (target) => normalizeGroupId(target),
    targetResolver: {
      looksLikeId: (input) => looksLikeGroupIdHex(normalizeGroupId(input)),
      hint: "<nostrGroupIdHex|marmot:group:<hex>>",
    },
  },

  outbound: {
    deliveryMode: "direct",
    textChunkLimit: 4000,
    sendText: async ({ to, text, accountId }) => {
      const aid = accountId ?? DEFAULT_ACCOUNT_ID;
      const handle = activeSidecars.get(aid);
      if (!handle) {
        throw new Error(`marmot sidecar not running for account ${aid}`);
      }
      const groupId = normalizeGroupId(to);
      if (!looksLikeGroupIdHex(groupId)) {
        throw new Error(`invalid marmot group id: ${to}`);
      }
      await handle.sidecar.sendMessage(groupId, text ?? "");
      return { channel: "marmot", to: groupId };
    },
  },

  gateway: {
    startAccount: async (ctx) => {
      const account = ctx.account;
      const runtime = getMarmotRuntime();
      const cfg = runtime.config.loadConfig();
      const resolved = resolveMarmotAccount({ cfg, accountId: account.accountId });

      if (!resolved.enabled) {
        throw new Error("marmot account disabled");
      }
      if (!resolved.configured) {
        throw new Error("marmot relays not configured (channels.marmot.relays)");
      }

      const relays = resolved.config.relays.map((r) => String(r).trim()).filter(Boolean);
      const baseStateDir = resolveAccountStateDir({
        accountId: resolved.accountId,
        stateDirOverride: resolved.config.stateDir,
      });
      const sidecarCmd = resolveSidecarCmd(resolved.config.sidecarCmd) ?? "rust_harness";
      const sidecarArgs =
        resolveSidecarArgs(resolved.config.sidecarArgs) ??
        ["daemon", "--relay", relays[0] ?? "ws://127.0.0.1:18080", "--state-dir", baseStateDir];

      ctx.log?.info(
        `[${resolved.accountId}] starting marmot sidecar cmd=${JSON.stringify(sidecarCmd)} args=${JSON.stringify(sidecarArgs)}`,
      );

      const sidecar = new MarmotSidecar({ cmd: sidecarCmd, args: sidecarArgs });
      const ready = await sidecar.waitForReady(15_000);
      activeSidecars.set(resolved.accountId, {
        sidecar,
        pubkey: ready.pubkey,
        npub: ready.npub,
      });
      ctx.setStatus({
        accountId: resolved.accountId,
        publicKey: ready.pubkey,
      });

      // Ensure the daemon has the full relay list (even if started with a single relay).
      await sidecar.setRelays(relays);
      await sidecar.publishKeypackage(relays);

      const groupPolicy = resolved.config.groupPolicy ?? "allowlist";
      const groupAllowFrom =
        (resolved.config.groupAllowFrom ?? []).map((x) => String(x).trim().toLowerCase()).filter(Boolean);
      const allowedGroups = resolved.config.groups ?? {};

      const isGroupAllowed = (nostrGroupId: string): boolean => {
        if (groupPolicy === "open") return true;
        const gid = String(nostrGroupId).trim().toLowerCase();
        return Boolean(allowedGroups[gid]);
      };
      const isSenderAllowed = (pubkey: string): boolean => {
        if (groupAllowFrom.length === 0) return true;
        const pk = String(pubkey).trim().toLowerCase();
        return groupAllowFrom.includes(pk);
      };

      sidecar.onEvent(async (ev) => {
        if (ev.type === "welcome_received") {
          ctx.log?.info(
            `[${resolved.accountId}] welcome_received from=${ev.from_pubkey} group=${ev.nostr_group_id} name=${JSON.stringify(ev.group_name)}`,
          );
          if (resolved.config.autoAcceptWelcomes) {
            try {
              await sidecar.acceptWelcome(ev.wrapper_event_id);
            } catch (err) {
              ctx.log?.debug(
                `[${resolved.accountId}] failed to accept welcome (stale?): ${err}`,
              );
            }
          }
          return;
        }
        if (ev.type === "group_joined") {
          ctx.log?.info(
            `[${resolved.accountId}] group_joined nostr_group_id=${ev.nostr_group_id} mls_group_id=${ev.mls_group_id}`,
          );
          return;
        }
        if (ev.type === "message_received") {
          if (!isGroupAllowed(ev.nostr_group_id)) {
            ctx.log?.debug(
              `[${resolved.accountId}] drop message (group not allowed) group=${ev.nostr_group_id}`,
            );
            return;
          }
          if (!isSenderAllowed(ev.from_pubkey)) {
            ctx.log?.debug(
              `[${resolved.accountId}] drop message (sender not allowed) sender=${ev.from_pubkey}`,
            );
            return;
          }

          const directive = parseReplyExactly(ev.content);
          if (directive !== null) {
            await sidecar.sendMessage(ev.nostr_group_id, directive);
            return;
          }

          try {
            await runtime.channel.reply.handleInboundMessage({
              channel: "marmot",
              accountId: resolved.accountId,
              senderId: ev.from_pubkey,
              chatType: "group",
              chatId: ev.nostr_group_id,
              text: ev.content,
              reply: async (responseText: string) => {
                await sidecar.sendMessage(ev.nostr_group_id, responseText);
              },
            });
          } catch (err) {
            ctx.log?.error(
              `[${resolved.accountId}] handleInboundMessage failed: ${err}`,
            );
          }
        }
      });

      return {
        stop: () => {
          const handle = activeSidecars.get(resolved.accountId);
          if (handle) {
            activeSidecars.delete(resolved.accountId);
            void handle.sidecar.shutdown();
          }
          ctx.log?.info(`[${resolved.accountId}] marmot sidecar stopped`);
        },
      };
    },
  },
};

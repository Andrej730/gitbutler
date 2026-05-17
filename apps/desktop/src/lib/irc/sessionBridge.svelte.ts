/**
 * IRC Session Bridge
 *
 * Bridges stack sessions to IRC, enabling:
 * - Broadcasting session state to IRC channels
 * - Session discovery via list requests
 *
 * Uses RTKQ (IrcApiService) for IRC mutations and IBackend.listen for
 * persistent message subscriptions.
 *
 * Note: This service must be instantiated within a Svelte component context
 * for the reactive effects to work properly.
 */

import { IRC_CONNECTION_ID, type IrcApiService } from "$lib/irc/ircApiService";
import {
	Messages,
	parse,
	parseTextCommand,
	serialize,
	sessionChannel,
	type IrcProtocolMessage,
	type PlainTextCommand,
} from "$lib/irc/protocol";
import { type SettingsService } from "$lib/settings/appSettings";
import { InjectionToken } from "@gitbutler/core/context";
import { persistWithExpiration, type Persisted } from "@gitbutler/shared/persisted";
import { reactive } from "@gitbutler/shared/reactiveUtils.svelte";
import { SvelteMap } from "svelte/reactivity";
import { get } from "svelte/store";
import type { IBackend } from "$lib/backend/backend";
import type { StoredMessage } from "$lib/irc/ircEndpoints";
import type { Reactive } from "@gitbutler/shared/storeUtils";

export const IRC_SESSION_BRIDGE = new InjectionToken<IrcSessionBridge>("IrcSessionBridge");

/**
 * Tracks an active session being bridged to IRC.
 */
type BridgedSession = {
	projectId: string;
	stackId: string;
	branchName?: string;
	channel: string;
	/** Whether the bridge is fully set up (IRC listeners active) */
	connected: boolean;
	/** Unsubscribe function for the IRC message listener */
	unsubscribeIrc?: () => void;
};

export class IrcSessionBridge {
	/** Active bridged sessions by stack ID */
	private sessions = new SvelteMap<string, BridgedSession>();
	/** Cached bot nick */
	private myNick: string | undefined;
	/** Per-stack persisted stores for manual bridging (keyed by stack ID, 7-day TTL) */
	private manualBridgeStores = new Map<string, Persisted<boolean>>();

	constructor(
		private backend: IBackend,
		private ircApiService: IrcApiService,
		private settingsService: SettingsService,
	) {}

	/** Get the user's IRC nick from settings (the account owner who may send commands). */
	private getOwnerNick(): string | undefined {
		return get(this.settingsService.appSettings)?.irc.connection.nickname ?? undefined;
	}

	/**
	 * Start bridging a session to IRC.
	 *
	 * Registers the session. The caller is responsible for driving
	 * connect/disconnect via `setBotReady`.
	 * Call `stopBridging` to tear everything down.
	 */
	startBridging(params: { projectId: string; stackId: string; branchName: string }): void {
		const { projectId, stackId, branchName } = params;
		const ownerNick = this.getOwnerNick() || "unknown";
		const channel = sessionChannel(ownerNick, branchName);

		if (this.sessions.has(stackId)) return;

		const session: BridgedSession = {
			projectId,
			stackId,
			branchName,
			channel,
			connected: false,
		};

		this.sessions.set(stackId, session);
	}

	/**
	 * Notify the bridge that the bot connection readiness changed.
	 * Connects or disconnects the bridge for the given session accordingly.
	 */
	setBotReady(stackId: string, ready: boolean): void {
		const session = this.sessions.get(stackId);
		if (!session) return;

		if (ready && !session.connected) {
			this.connectBridge(session);
		} else if (!ready && session.connected) {
			this.disconnectBridge(session);
		}
	}

	/**
	 * Activate the bridge for a session: join channel, subscribe to messages.
	 */
	private async connectBridge(session: BridgedSession): Promise<void> {
		if (session.connected) return;
		session.connected = true;

		try {
			this.myNick = await this.ircApiService.fetchNick();
		} catch {
			// Non-critical
		}

		try {
			await this.ircApiService.joinChannel({ channel: session.channel });
		} catch (e) {
			console.warn("[IrcSessionBridge] Failed to join channel:", e);
		}

		session.unsubscribeIrc = wrapUnsubscribe(
			this.backend.listen<StoredMessage>(`irc:${IRC_CONNECTION_ID}:message`, (event) => {
				const msg = event.payload;
				if (msg.target !== session.channel) return;

				// Only accept messages from the session owner (same nick via bouncer/multi-client)
				const ownerNick = this.getOwnerNick();
				if (ownerNick && msg.sender !== ownerNick) return;

				// Skip protocol messages (bridge's own publish() calls carry +data).
				if (msg.data) return;

				this.handleNewIrcMessage(session, {
					channel: msg.target,
					from: msg.sender,
					text: msg.content,
					data: msg.data ?? undefined,
				});
			}),
		);
	}

	/**
	 * Deactivate the bridge for a session: unsubscribe from messages, part channel.
	 */
	private disconnectBridge(session: BridgedSession): void {
		session.unsubscribeIrc?.();
		session.unsubscribeIrc = undefined;
		session.connected = false;
	}

	/**
	 * Stop bridging a session.
	 */
	stopBridging(stackId: string, exitCode: number = 0): void {
		const session = this.sessions.get(stackId);

		if (!session) return;

		// Tear down the active bridge if connected
		if (session.connected) {
			this.disconnectBridge(session);

			// Announce session end and leave channel
			this.publish(session.channel, Messages.sessionEnd({ code: exitCode }));
			this.ircApiService.partChannel({ channel: session.channel }).catch(() => {});
		}

		// Cleanup - create new map to trigger reactivity
		this.sessions.delete(stackId);
	}

	/**
	 * Publish a protocol message to an IRC channel.
	 */
	private publish(channel: string, message: IrcProtocolMessage): void {
		const { text, data, truncated } = serialize(message);
		this.sendToChannel(channel, text, data);

		if (truncated) {
			console.warn(`[IrcSessionBridge] Message truncated for ${channel}:`, message.type);
		}
	}

	/**
	 * Send a message to an IRC channel, optionally with a data payload.
	 */
	private sendToChannel(channel: string, message: string, data?: unknown): void {
		// Never send empty/whitespace-only text messages
		if (!data && !message.trim()) return;

		if (data) {
			const encoded = typeof data === "string" ? data : JSON.stringify(data);
			this.ircApiService
				.sendMessageWithData({
					target: channel,
					message,
					data: encoded,
				})
				.catch((e) => {
					console.warn("[IrcSessionBridge] Failed to send message with data:", e);
				});
		} else {
			this.ircApiService
				.sendMessage({
					target: channel,
					message,
				})
				.catch((e) => {
					console.warn("[IrcSessionBridge] Failed to send message:", e);
				});
		}
	}

	/**
	 * Handle a new incoming IRC message.
	 * Only accepts commands from the configured user nick (session owner).
	 */
	private handleNewIrcMessage(
		session: BridgedSession,
		msg: { channel: string; from: string; text: string; data?: string },
	): void {
		// Try to parse as protocol message first (from +data tag)
		if (msg.data) {
			try {
				const parsed = parse(msg.data);
				if (parsed) {
					this.handleProtocolMessage(session, parsed);
					return;
				}
			} catch {
				// Failed to decode/parse, fall through to text command parsing
			}
		}

		// Only process explicit bang commands (!prompt, !approve, etc.)
		if (!msg.text.trimStart().startsWith("!")) return;

		const command = parseTextCommand(msg.text);
		if (command.type !== "unknown") {
			this.handleTextCommand(session, command);
		}
	}

	/**
	 * Handle a parsed protocol message from IRC.
	 */
	private handleProtocolMessage(session: BridgedSession, message: IrcProtocolMessage): void {
		switch (message.type) {
			case "session-list-request":
				this.handleSessionListRequest(session.channel, message.payload.projectId);
				break;
		}
	}

	/**
	 * Handle a plain text command from IRC.
	 */
	private handleTextCommand(session: BridgedSession, command: PlainTextCommand): void {
		switch (command.type) {
			case "sessions":
				this.handleSessionListRequest(session.channel, command.projectId);
				break;
		}
	}

	// =========================================================================
	// Session Discovery
	// =========================================================================

	/**
	 * Handle a session list request.
	 */
	private handleSessionListRequest(responseChannel: string, filterProjectId?: string): void {
		const sessions = Array.from(this.sessions.values())
			.filter((s) => !filterProjectId || s.projectId === filterProjectId)
			.map((s) => ({
				projectId: s.projectId,
				stackId: s.stackId,
				branchName: s.branchName,
				status: "enabled" as const,
				channel: s.channel,
			}));

		this.publish(
			responseChannel,
			Messages.sessionListResponse({
				sessions,
			}),
		);
	}

	/**
	 * Clean up all bridged sessions.
	 */
	destroy(): void {
		for (const session of this.sessions.values()) {
			this.disconnectBridge(session);
			this.ircApiService.partChannel({ channel: session.channel }).catch(() => {});
		}
		this.sessions.clear();
	}

	/**
	 * Get the list of currently bridged sessions.
	 */
	getBridgedSessions(): BridgedSession[] {
		return Array.from(this.sessions.values());
	}

	/**
	 * Check if a session is being bridged.
	 */
	isBridging(stackId?: string): Reactive<boolean> {
		return reactive(() => (stackId ? this.sessions.has(stackId) : false));
	}

	/**
	 * Get or create the persisted store for a stack's manual bridge state.
	 */
	private getManualBridgeStore(stackId: string): Persisted<boolean> {
		let store = this.manualBridgeStores.get(stackId);
		if (!store) {
			store = persistWithExpiration<boolean>(false, `irc:manualBridge:${stackId}`, 60 * 24 * 7);
			this.manualBridgeStores.set(stackId, store);
		}
		return store;
	}

	/**
	 * Enable or disable manual bridging for a stack (persisted across reloads).
	 */
	setManualBridge(stackId: string, enabled: boolean): void {
		this.getManualBridgeStore(stackId).set(enabled);
	}

	/**
	 * Check if a stack has been manually enabled for bridging.
	 */
	isManuallyBridged(stackId?: string): Reactive<boolean> {
		if (!stackId) return reactive(() => false);
		const store = this.getManualBridgeStore(stackId);
		return reactive(() => get(store));
	}
}

// ============================================================================
// Helpers
// ============================================================================

/** Wrap an async listener result into a synchronous unsubscribe function. */
function wrapUnsubscribe(listenResult: Promise<() => void> | (() => void)): () => void {
	return () => {
		Promise.resolve(listenResult).then((fn) => {
			if (typeof fn === "function") fn();
		});
	};
}

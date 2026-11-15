// Public protocol-v2 Tauri IPC. Timestamp fields are Unix milliseconds.
// Pairing verification and secrets belong only to local human interfaces; never project tickets/tokens to Agents.
export type ModuleId = 'system' | 'windows' | 'mobile';
export type ModulePhase = 'installed' | 'starting' | 'ready' | 'disabled' | 'failed';
export interface ModuleStatus { id: ModuleId; phase: ModulePhase; version: string; error: string | null }
export interface ClientInfo { id: string; name: string; revoked: boolean }
export interface PairRequest { id: string; clientName: string; clientIdentity: string; verificationCode: string; createdAt: number }
export interface JobInfo {
  id: string; requestId: string; clientId: string; chatId: string;
  state: 'queued' | 'running' | 'succeeded' | 'failed' | 'timed_out' | 'cancel_requested' | 'cancelled' | 'interrupted' | 'unknown';
  exitCode: number | null; createdAt: number; expiresAt: number;
}
export interface PhoneInfo { id: string; name: string; transports: string[]; activeTransport: string | null; state: string; reason: string | null; removed: boolean }
export interface ControlInfo { id: string; module: 'windows' | 'mobile'; resource: string; clientId: string; chatId: string; operation: string; startedAt: number }
export interface DeviceSnapshot {
  protocolVersion: 2;
  identity: { hostId: string; displayName: string; publicKeyPin: string };
  service: { state: 'starting' | 'ready' | 'failed' | 'stopped'; address: string | null; error: string | null };
  modules: ModuleStatus[]; clients: ClientInfo[]; pairing: PairRequest[];
  phones: PhoneInfo[]; controls: ControlInfo[]; jobs: JobInfo[];
}
export interface Commands {
  device_snapshot: { args: Record<string, never>; result: DeviceSnapshot };
  device_module: { args: { module: ModuleId; enabled: boolean }; result: DeviceSnapshot };
  device_pair_decide: { args: { requestId: string; accept: boolean }; result: DeviceSnapshot };
  device_client_revoke: { args: { clientId: string }; result: DeviceSnapshot };
  device_stop_control: { args: { clientId?: string | null; phoneId?: string | null }; result: DeviceSnapshot };
  device_phone_remove: { args: { phoneId: string }; result: DeviceSnapshot };
  device_phone_add: { args: { phoneId: string }; result: DeviceSnapshot };
  device_phone_refresh: { args: Record<string, never>; result: DeviceSnapshot };
  // Local human IPC only. Never expose pairCode to MCP, logs, snapshots or Agents.
  device_phone_pair: { args: { address: string; pairCode: string }; result: DeviceSnapshot };
  device_phone_connect: { args: { address: string }; result: DeviceSnapshot };
  device_exit: { args: Record<string, never>; result: null };
}
export const snapshotEvent = 'maxa-device:snapshot' as const;
// Module version stays empty until the private component pack is verified and installed.
// Rejected commands return string errors; never substitute a successful state.

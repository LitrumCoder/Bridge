import { invoke } from '@tauri-apps/api/core';

import type { Request, Response } from './types';

/** Sends one request to the daemon through the app's Rust side; errors become exceptions. */
export async function call(request: Request): Promise<Response> {
  const response = await invoke<Response>('ipc', { request });
  if (typeof response === 'object' && 'Error' in response) {
    throw new Error(response.Error);
  }
  return response;
}

/** Narrows a response to one variant, or throws if the daemon answered something else. */
export function expect<K extends string>(response: Response, key: K): Extract<Response, Record<K, unknown>>[K] {
  if (typeof response === 'object' && key in response) {
    return (response as unknown as Record<K, never>)[key];
  }
  throw new Error(`unexpected reply: ${JSON.stringify(response)}`);
}

export async function qrSvg(text: string): Promise<string> {
  return invoke<string>('qr_svg', { text });
}

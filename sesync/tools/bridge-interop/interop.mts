// Interop check: Orca's own TypeScript (pairing offer codec and schema, compatibility gate, host
// status schema, method-unavailable rule, and the mock server's E2EE framing) against a running
// `sesync bridge serve`. The Orca files are copied, unchanged, next to this script so their imports
// resolve against this package's pinned zod and tweetnacl.
//
//   ORCA_PHONE_DIR=~/projects/orca-phone npm run interop -- "orca://pair?code=…"
//
// Exit 0 only when every check passes. The output never includes the device token.

import { cpSync, existsSync, rmSync } from 'node:fs'
import { homedir } from 'node:os'
import { dirname, join, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import { z } from 'zod'

const here = dirname(fileURLToPath(import.meta.url))
const orcaDir = resolve(process.env.ORCA_PHONE_DIR ?? join(homedir(), 'projects/orca-phone'))

// Orca files used here, by path inside the orca-phone repo. Copied with the same relative layout.
const orcaFiles = [
  'src/shared/pairing.ts',
  'src/shared/mobile-relay-pairing-offer.ts',
  'src/shared/mobile-pairing-base64.ts',
  'src/shared/mobile-pairing-protocol-limits.ts',
  'src/shared/runtime-host-descriptor.ts',
  'src/shared/mobile-relay-credential-contract.ts',
  'src/shared/zod-salvage.ts',
  'mobile/src/transport/protocol-version.ts',
  'mobile/src/transport/protocol-compat.ts',
  'mobile/src/transport/host-status-reply-schema.ts',
  'mobile/src/transport/mobile-runtime-host-platform.ts',
  'mobile/src/transport/mobile-method-unavailable.ts',
  'mobile/scripts/mock-server-encryption.ts',
]

const copyDir = join(here, 'orca')
rmSync(copyDir, { recursive: true, force: true })
for (const rel of orcaFiles) {
  const src = join(orcaDir, rel)
  if (!existsSync(src)) {
    console.error(`missing Orca file: ${src} (set ORCA_PHONE_DIR)`)
    process.exit(2)
  }
  cpSync(src, join(copyDir, rel))
}

const pairing = await import('./orca/src/shared/pairing.ts')
const compat = await import('./orca/mobile/src/transport/protocol-compat.ts')
const hostStatus = await import('./orca/mobile/src/transport/host-status-reply-schema.ts')
const unavailable = await import('./orca/mobile/src/transport/mobile-method-unavailable.ts')
const framing = await import('./orca/mobile/scripts/mock-server-encryption.ts')
const { default: nacl } = await import('tweetnacl')

// Every reply from the bridge is parsed here before a check reads it.
const Reply = z.looseObject({
  type: z.string().optional(),
  ok: z.boolean().optional(),
  result: z.unknown().optional(),
  // Orca's auth refusal is {code:'unauthorized'} with no message; RPC errors carry one.
  error: z.looseObject({ code: z.string(), message: z.string().optional() }).optional(),
})
const WorktreeResult = z.looseObject({ worktrees: z.array(z.unknown()) })
const RepoResult = z.looseObject({ repos: z.array(z.unknown()) })
const StatusResult = z.looseObject({
  protocolVersion: z.number(),
  minCompatibleMobileVersion: z.number(),
  capabilities: z.array(z.string()),
  floatingWorkspaceEnabled: z.boolean(),
})

type Check = { check: string; ok: boolean; detail?: string }
const checks: Check[] = []
function record(check: string, ok: boolean, detail?: string): void {
  const entry: Check = detail === undefined ? { check, ok } : { check, ok, detail }
  checks.push(entry)
  console.log(JSON.stringify(entry))
}

function parseReply(text: string): z.infer<typeof Reply> {
  return Reply.parse(JSON.parse(text))
}

type Event = { kind: 'message'; data: string } | { kind: 'close'; code: number }

// One WebSocket with a queue, so a check can await the next message or close in order.
class Wire {
  readonly ws: WebSocket
  private readonly queue: Event[] = []
  private waiter: ((event: Event) => void) | null = null
  private sharedKey: Uint8Array | null = null

  constructor(url: string) {
    this.ws = new WebSocket(url)
    this.ws.onmessage = (ev) => this.push({ kind: 'message', data: String(ev.data) })
    this.ws.onclose = (ev) => this.push({ kind: 'close', code: ev.code })
  }

  setKey(key: Uint8Array): void {
    this.sharedKey = key
  }

  private key(): Uint8Array {
    if (!this.sharedKey) throw new Error('no shared key yet: hello has not completed')
    return this.sharedKey
  }

  private push(event: Event): void {
    if (this.waiter) {
      const wake = this.waiter
      this.waiter = null
      wake(event)
    } else {
      this.queue.push(event)
    }
  }

  open(): Promise<void> {
    return new Promise((resolveOpen, rejectOpen) => {
      this.ws.onopen = () => resolveOpen()
      this.ws.addEventListener('error', () => rejectOpen(new Error('connection failed')), { once: true })
    })
  }

  next(ms = 5000): Promise<Event> {
    const queued = this.queue.shift()
    if (queued) return Promise.resolve(queued)
    return new Promise((resolveNext, rejectNext) => {
      const timer = setTimeout(() => rejectNext(new Error('no answer in time')), ms)
      this.waiter = (event) => {
        clearTimeout(timer)
        resolveNext(event)
      }
    })
  }

  async plaintextReply(): Promise<z.infer<typeof Reply>> {
    const event = await this.next()
    if (event.kind !== 'message') throw new Error(`expected a message, got close ${event.code}`)
    return parseReply(event.data)
  }

  async sealedReply(): Promise<z.infer<typeof Reply>> {
    const event = await this.next()
    if (event.kind !== 'message') throw new Error(`expected a message, got close ${event.code}`)
    const plain = framing.e2eeDecrypt(event.data, this.key())
    if (plain === null) throw new Error('reply does not authenticate under the shared key')
    return parseReply(plain)
  }

  sendSealed(value: unknown): void {
    this.ws.send(framing.e2eeEncrypt(JSON.stringify(value), this.key()))
  }

  async closeCode(): Promise<number> {
    const event = await this.next()
    if (event.kind !== 'close') throw new Error(`expected a close, got a message`)
    return event.code
  }
}

// Orca's mock client: plaintext hello with the client's public key, ready, then the shared key
// nacl.box.before(serverPublic, clientSecret) from the pinned server key.
async function hello(endpoint: string, serverPublic: Uint8Array): Promise<Wire> {
  const wire = new Wire(endpoint)
  await wire.open()
  const client = nacl.box.keyPair()
  wire.ws.send(JSON.stringify({ type: 'e2ee_hello', publicKeyB64: Buffer.from(client.publicKey).toString('base64') }))
  const ready = await wire.plaintextReply()
  if (ready.type !== 'e2ee_ready') throw new Error(`expected e2ee_ready, got type ${ready.type ?? 'none'}`)
  wire.setKey(framing.deriveSharedKey(client.secretKey, serverPublic))
  return wire
}

const url = process.argv[2]
if (!url || !url.startsWith('orca://pair')) {
  console.error('usage: npm run interop -- "orca://pair?code=…"   (the url sesync bridge pair prints)')
  process.exit(2)
}

// 1. Orca's own codec and schema accept the offer. decodePairingOffer throws on a schema refusal.
const offer = (() => {
  try {
    return pairing.decodePairingOffer(url)
  } catch (error) {
    record('Orca PairingOfferSchema accepts the offer', false, error instanceof Error ? error.message : String(error))
    process.exit(1)
  }
})()
record('Orca PairingOfferSchema accepts the offer', offer.v === 2 && offer.scope === undefined && offer.relay === undefined)
const serverPublic = Uint8Array.from(Buffer.from(offer.publicKeyB64, 'base64'))
record('offer public key is 32 bytes', serverPublic.length === 32)

// 2. Pairing: legacy framing, auth, the status gate Orca applies.
const paired = await hello(offer.endpoint, serverPublic)
record('hello answered with e2ee_ready (legacy framing)', true)
paired.sendSealed({ type: 'e2ee_auth', deviceToken: offer.deviceToken })
const authReply = await paired.sealedReply()
record('paired token authenticates', authReply.type === 'e2ee_authenticated', `type ${authReply.type ?? 'none'}`)

paired.sendSealed({ id: 'rpc-status', deviceToken: offer.deviceToken, method: 'status.get' })
const status = await paired.sealedReply()
const statusResult = StatusResult.safeParse(status.result)
record('status.get ok with the fields the app reads', status.ok === true && statusResult.success)
if (statusResult.success) {
  const gate = compat.evaluateCompat({
    desktopProtocolVersion: statusResult.data.protocolVersion,
    desktopMinCompatibleMobileVersion: statusResult.data.minCompatibleMobileVersion,
  })
  record('Orca compatibility gate passes', gate.kind === 'ok', gate.kind)
  record('Orca host status schema reads the result', hostStatus.hostStatusSchema.safeParse(statusResult.data).success)
}

// 3. The home screen's reads. Served: worktree.ps and repo.list. Unserved: method_not_found, which
// Orca must read as "unavailable" and skip.
paired.sendSealed({ id: 'rpc-wt', deviceToken: offer.deviceToken, method: 'worktree.ps' })
const worktrees = await paired.sealedReply()
record('worktree.ps returns a worktree list', worktrees.ok === true && WorktreeResult.safeParse(worktrees.result).success)

paired.sendSealed({ id: 'rpc-repo', deviceToken: offer.deviceToken, method: 'repo.list' })
const repos = await paired.sealedReply()
record('repo.list returns a repo list', repos.ok === true && RepoResult.safeParse(repos.result).success)

paired.sendSealed({ id: 'rpc-term', deviceToken: offer.deviceToken, method: 'terminal.list' })
const term = await paired.sealedReply()
const termCode = term.error?.code
const termMessage = term.error?.message
record(
  'unserved method reads as unavailable to Orca',
  term.ok === false && unavailable.isMobileMethodUnavailableError(termCode, termMessage),
  termCode ?? 'no error code',
)
paired.ws.close()

// 4. A device token that is not paired is refused with unauthorized and a 4001 close.
const unpaired = await hello(offer.endpoint, serverPublic)
unpaired.sendSealed({ type: 'e2ee_auth', deviceToken: 'A'.repeat(43) })
const refused = await unpaired.sealedReply()
const refusedCode = refused.error?.code
record('unpaired token is refused as unauthorized', refused.type === 'e2ee_error' && refusedCode === 'unauthorized')
record('unpaired token closes with 4001', (await unpaired.closeCode()) === 4001)

// 5. A v2 hello is refused before ready with 4001: the app's cue to fall back to legacy framing.
const v2 = new Wire(offer.endpoint)
await v2.open()
v2.ws.send(JSON.stringify({
  type: 'e2ee_hello',
  v: 2,
  publicKeyB64: Buffer.from(nacl.box.keyPair().publicKey).toString('base64'),
  clientNonceB64: Buffer.alloc(32).toString('base64'),
  capabilities: { framing: [2], payloadKinds: ['text', 'binary'] },
  context: { protocol: 'orca-mobile-e2ee', initiator: 'mobile', responder: 'desktop', transport: 'direct' },
}))
const v2Close = await v2.closeCode()
record('v2 hello is closed with 4001 before ready', v2Close === 4001, String(v2Close))

const failed = checks.filter((c) => !c.ok)
console.log(JSON.stringify({ summary: { total: checks.length, failed: failed.length } }))
process.exit(failed.length === 0 ? 0 : 1)

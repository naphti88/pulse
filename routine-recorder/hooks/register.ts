import type { EngineInterface, Register } from 'claude-code'

// Tells Pulse when any session stands before a person: a permission dialog or
// a question of an MCP server, in ~/.claude/routine-recorder/pending/<sessionId>.json
// while it lasts. Its name only: never the input of a tool or what is asked.
//
// Tells Pulse when an API error ended a turn, in
// ~/.claude/routine-recorder/failed/<sessionId>.json until the next turn: the
// kind of error only, never its text.
//
// Records each run of a scheduled routine into one JSON file per session,
// ~/.claude/routine-recorder/runs/<sessionId>.json, which the routine
// dashboard reads. A session becomes a routine run when a scheduled task
// fires its prompt into it; every later turn of that session (the person
// continuing in the run's chat) is recorded too, marked by who started it.
// Every session, a routine run or not, is asked to end its answers with a
// STATUS line (the Claude app shows HTML comments and markdown references,
// so it is a plain line). It stays in the transcript, where Pulse reads it, and
// is taken off what the app shows of the answer (classic.MessageDisplay).

type $ = EngineInterface

export type RunStatus = 'idle' | 'worked' | 'waiting' | 'blocked' | 'parked'
export type TurnBy = 'routine' | 'user' | 'system'
export type Trigger = 'scheduled' | 'manual'

export type Ping = { via: string; detail: string; at: number }

export type Permission = {
  tool: string
  toolUseId?: string
  decision: 'ask' | 'deny'
  reason?: string
  rule?: string
  at: number
  outcome?: 'allowed' | 'denied' | 'unanswered' | 'error'
  outcomeText?: string
}

export type Turn = {
  id: string
  by: TurnBy
  trigger?: Trigger
  startedAt: number
  endedAt?: number
  end?: 'answer' | 'aborted' | 'refusal' | 'error'
  status?: RunStatus
  statusText?: string
  answerTail?: string
  pings: Ping[]
  tools: Record<string, number>
  toolErrors: number
  lastError?: string
  permissions: Permission[]
  interjections: number
}

export type RunRecord = {
  version: 1
  // live: written by this plugin; backfill: rebuilt from an older transcript;
  // backfill+live: a backfilled run this plugin went on recording
  source?: 'live' | 'backfill' | 'backfill+live'
  sessionId: string
  taskId: string
  taskFile?: string
  cwd: string
  startedAt: number
  updatedAt: number
  activeTurnId: string | null
  turns: Turn[]
}

const TRIGGER = /^\s*<scheduled-task name="([^"]+)"(?: file="([^"]+)")?/
// the plain line as asked for, or as a markdown reference ("[//]: # (...)")
// or an HTML comment, as asked for for a while: each form with its own end,
// so a plain line keeps a final ")", and a line with nothing after the word
// takes nothing from the next line
const STATUS_LINE =
  /^[ \t>*_`]*(?:<!--\s*STATUS:\s*(idle|worked|waiting|blocked|parked)\b[^\S\r\n]*(?:[|:–]|-(?!->))?[^\S\r\n]*(.*?)\s*(?:-->)?|\[\/\/\]:\s*#\s*\(\s*STATUS:\s*(idle|worked|waiting|blocked|parked)\b[^\S\r\n]*[|:–-]?[^\S\r\n]*(.*?)\s*\)?|STATUS:\s*(idle|worked|waiting|blocked|parked)\b[^\S\r\n]*[|:–-]?[^\S\r\n]*(.*?))[\s*_`]*$/gim
const PING_LINE =
  /^[ \t>*_`]*(?:<!--\s*PING:[^\S\r\n]*(\S.*?)\s*(?:-->)?|\[\/\/\]:\s*#\s*\(\s*PING:[^\S\r\n]*(\S.*?)\s*\)?|PING:[^\S\r\n]*(\S.*?))[\s*_`]*$/gim
const PING_TOOL = /^PushNotification$|^mcp__.+__(send|post|notify|reply|chat_post)/i
const WEBHOOK = /discord(?:app)?\.com\/api\/webhooks|hooks\.slack\.com/i
const UNANSWERED_TEXT = /went unanswered/i
const DENIED_TEXT = /doesn't want to proceed|permission.{0,40}denied|was denied|rejected/i

// how much of a round's answer is kept: enough for its closing sentences
const TAIL = 600
const MAX_TURNS = 200
const MAX_PER_TURN = 50

// texts are cut by characters, as backfill_runs.py cuts them: a cut by UTF-16
// units can split an emoji, and Pulse can't read a record with half of one
const cut = (text: string, from: number, to?: number) => Array.from(text).slice(from, to).join('')

// Every session ends its answers with a STATUS line, which Pulse reads
// from the transcript (a chat) or from the record (a routine run). worked is a
// result the person only reads; waiting stays open after they read it
export function statusPrompt(isRoutine: boolean): string {
  return [
    isRoutine
      ? "This session is a run of one of the user's scheduled routines, and the user's dashboard (Pulse) reads its answers."
      : "The user's dashboard (Pulse) reads the answers of this session to show them what needs them.",
    'End every answer you give in this session with an empty line and then one final line in exactly this format:',
    "STATUS: <idle|worked|waiting|blocked|parked> | <one short sentence in the user's language>",
    'worked: the task is finished; the user may read the result, but nothing is left for them to answer, decide or do. Once they have read it, it is closed.',
    'waiting: the user has to answer a question or make a decision, or parts of the task are still open (the sentence says which). Reading the answer does not settle it.',
    'A finished task whose result reports a problem the user has to act on, or a recommendation they have to approve or choose from, is waiting, not worked: they must not lose it once they have read it. When unsure between the two, it is waiting.',
    'blocked: you could not do the work (a permission was refused, access or a tool was missing, an error stopped you).',
    'idle: there was nothing to do.',
    'parked: the user asked to set this topic aside for later (to park it). Before the STATUS line, sum up in a few lines where it stands and what the next step would be; the sentence says that in short. Only when the user asks for it.',
    ...(isRoutine
      ? [
          'If during this answer you notified anyone outside this chat (push notification, Discord, Slack, email, a message sent through an API), put a line "PING: <who and what>" directly before the STATUS line, after the empty line.',
        ]
      : []),
  ].join('\n')
}

// what stands before the person now: the tool it wants; a question of an MCP
// server has no tool of its own (mcp__ stands for the call it came in); agent:
// asked by a subagent, whose calls the session's transcript does not show;
// checked: the engine only said the call must be asked for, and in auto mode a
// classifier may still let it through by itself. The app's own dialogs (the
// built-in browser) tell nothing else: neither the process nor a PermissionRequest
export type Pending = { tool: string; at: number; agent?: string; asks?: true; checked?: true }

type State = {
  // what stands before the person now, oldest first: calls run side by side,
  // and one that ends says nothing of another that still waits
  pending: Pending[]
  loaded?: Promise<RunRecord | null>
  rec: RunRecord | null
  file: string
  saving: Promise<void>
  pendingBy?: TurnBy
  pendingTrigger?: Trigger
}

// Claude Code (os.homedir) and Pulse use USERPROFILE on Windows even when
// HOME is set (Git Bash, MSYS); OS=Windows_NT is set on every Windows box.
// Elsewhere HOME comes first. With neither, nothing is recorded: the path
// would be relative and leave an undefined folder in the session's project.
async function homeOf($: $): Promise<string | undefined> {
  const win = (await $.env.get('OS')) === 'Windows_NT'
  const profile = await $.env.get('USERPROFILE')
  const posix = await $.env.get('HOME')
  return (win ? profile || posix : posix || profile) || undefined
}

// a file of the recorder's own; without a home folder there is none to write
async function recorderFile($: $, folder: string): Promise<string> {
  const home = await homeOf($)
  if (!home) throw new Error('no home folder')
  return `${home}/.claude/routine-recorder/${folder}/${await $.session.id()}.json`
}

async function load($: $, s: State): Promise<RunRecord | null> {
  const home = await homeOf($)
  if (!home) return null
  const id = await $.session.id()
  s.file = `${home}/.claude/routine-recorder/runs/${id}.json`
  if (!(await $.fs.exists(s.file))) return null
  const rec = JSON.parse(await $.fs.read(s.file)) as RunRecord
  if (rec.source === 'backfill') rec.source = 'backfill+live'
  s.rec = rec
  return rec
}

async function ensure($: $, s: State): Promise<RunRecord | null> {
  s.loaded ??= load($, s).catch(() => null)
  await s.loaded
  return s.rec
}

async function save($: $, s: State): Promise<void> {
  // no file to write without a home folder, or when load failed before naming it
  if (s.rec === null || s.file === '') return
  s.rec.updatedAt = await $.clock.now()
  const text = JSON.stringify(s.rec, null, 1)
  s.saving = s.saving.then(() => $.fs.write(s.file, text)).catch(() => undefined)
  await s.saving
}

async function pendingFile($: $): Promise<string> {
  return recorderFile($, 'pending')
}

async function writePending($: $, s: State): Promise<void> {
  try {
    await $.fs.write(await pendingFile($), JSON.stringify(s.pending))
  } catch {
    // Pulse falls back on what the transcript shows, and trusts none of this
    // once the call is over
  }
}

// one entry a tool: a request can be told more than once
async function addPending($: $, s: State, pending: Omit<Pending, 'at'>): Promise<void> {
  if (s.pending.some(p => p.tool === pending.tool && p.agent === pending.agent)) return
  s.pending.push({ ...pending, at: await $.clock.now() })
  await writePending($, s)
}

// a call ended: what it asked for is answered, whatever the others wait for
async function endCall($: $, s: State, tool: string): Promise<void> {
  const at = s.pending.findIndex(p => p.tool === tool || (p.asks === true && tool.startsWith(p.tool)))
  if (at === -1) return
  s.pending.splice(at, 1)
  await writePending($, s)
}

async function failedFile($: $): Promise<string> {
  return recorderFile($, 'failed')
}

async function clearFailed($: $): Promise<void> {
  try {
    const file = await failedFile($)
    if (await $.fs.exists(file)) await $.fs.write(file, 'null')
  } catch {
    // a process that says it works outweighs this in Pulse
  }
}

// an answer as it is shown: without the STATUS line it ends with; a line like
// it anywhere else is part of what was said
export function withoutStatus(text: string): string {
  const lines = text.trimEnd().split('\n')
  if ([...(lines.at(-1) ?? '').matchAll(STATUS_LINE)].length === 0) return text
  return lines.slice(0, -1).join('\n').trimEnd()
}

async function clearPending($: $, s: State, keep: (p: Pending) => boolean = () => false): Promise<void> {
  const left = s.pending.filter(keep)
  if (left.length === s.pending.length) return
  s.pending = left
  await writePending($, s)
}

function current(rec: RunRecord): Turn | undefined {
  return rec.turns.find(t => t.id === rec.activeTurnId) ?? rec.turns.at(-1)
}

export const register: Register = on => {
  const s: State = { pending: [], rec: null, file: '', saving: Promise.resolve() }

  on('prompt.submit', async ($, e, next) => {
    try {
      let rec = await ensure($, s)
      const trigger = TRIGGER.exec(e.text)
      const isTrigger = trigger !== null || e.origin.kind === 'scheduled-trigger'
      if (isTrigger && rec === null) {
        const now = await $.clock.now()
        rec = s.rec = {
          version: 1,
          source: 'live',
          sessionId: await $.session.id(),
          taskId: trigger?.[1] ?? 'unknown',
          taskFile: trigger?.[2],
          cwd: await $.session.cwd(),
          startedAt: now,
          updatedAt: now,
          activeTurnId: null,
          turns: [],
        }
      }
      if (rec !== null) {
        const by: TurnBy = isTrigger
          ? 'routine'
          : ['task-notification', 'peer', 'peer-send-message'].includes(e.origin.kind)
            ? 'system'
            : 'user'
        const running = e.turnId === undefined ? undefined : rec.turns.find(t => t.id === e.turnId)
        if (running !== undefined) running.interjections++
        else if (e.text.trimStart().startsWith('<bash-input>')) {
          // a command the person ran from the chat (the Play button, "!") is a
          // round of theirs even when nothing answers it: what was refused
          // before is theirs to have handled
          const now = await $.clock.now()
          rec.turns.push({
            id: `command-${now}`,
            by: 'user',
            startedAt: now,
            endedAt: now,
            end: 'answer',
            pings: [],
            tools: {},
            toolErrors: 0,
            permissions: [],
            interjections: 0,
          })
          rec.turns = rec.turns.slice(-MAX_TURNS)
          s.pendingBy = 'user'
          s.pendingTrigger = undefined
        } else {
          s.pendingBy = by
          s.pendingTrigger = !isTrigger
            ? undefined
            : ['scheduled-trigger', 'task-notification'].includes(e.origin.kind)
              ? 'scheduled'
              : 'manual'
        }
        await save($, s)
      }
    } catch {
      // the recorder never stands in the way of a prompt
    }
    // The model reads the STATUS instruction beside every prompt: the system
    // prompt is composed before a session is known to be a routine run.
    return next({ ...e, context: [...(e.context ?? []), statusPrompt(s.rec !== null)] })
  }).catch(($, e, next) => next(e))

  on('turn.start', async ($, e, next) => {
    if (e.agentId !== undefined) return next(e)
    await clearFailed($)
    try {
      const rec = await ensure($, s)
      if (rec !== null) {
        rec.turns.push({
          id: e.turnId,
          by: s.pendingBy ?? 'system',
          trigger: s.pendingTrigger,
          startedAt: await $.clock.now(),
          pings: [],
          tools: {},
          toolErrors: 0,
          permissions: [],
          interjections: 0,
        })
        rec.turns = rec.turns.slice(-MAX_TURNS)
        rec.activeTurnId = e.turnId
        s.pendingBy = undefined
        s.pendingTrigger = undefined
        await save($, s)
      }
    } catch {
      // keep the turn going
    }
    return next(e)
  })

  on('classic.PermissionRequest', async ($, e, next) => {
    await addPending($, s, { tool: e.tool_name, ...(e.agent_id === undefined ? {} : { agent: e.agent_id }) })
    return next(e)
  }).catch(($, e, next) => next(e))

  on('classic.StopFailure', async ($, e, next) => {
    if (e.agent_id === undefined) {
      try {
        await $.fs.write(await failedFile($), JSON.stringify({ error: e.error, at: await $.clock.now() }))
      } catch {
        // Pulse then goes by the last answer
      }
    }
    return next(e)
  }).catch(($, e, next) => next(e))

  // only what is shown: the stored answer and what the model reads keep the line
  on('classic.MessageDisplay', async ($, e, next) => {
    const done = await next(e)
    if (!e.final) return done
    const shown = withoutStatus(e.delta)
    return shown === e.delta ? done : { ...done, displayContent: shown }
  }).catch(($, e, next) => next(e))

  on('classic.Elicitation', async ($, e, next) => {
    await addPending($, s, { tool: 'mcp__', asks: true })
    return next(e)
  }).catch(($, e, next) => next(e))

  on('classic.ElicitationResult', async ($, e, next) => {
    await clearPending($, s, p => p.asks !== true)
    return next(e)
  }).catch(($, e, next) => next(e))

  on('tool.check', async ($, e, next) => {
    const verdict = await next(e)
    if (verdict.decision === 'ask') {
      await addPending($, s, { tool: e.tool, checked: true, ...(e.agentId === undefined ? {} : { agent: e.agentId }) })
    }
    try {
      const rec = await ensure($, s)
      const turn = rec === null ? undefined : current(rec)
      if (turn !== undefined && verdict.decision !== 'allow') {
        turn.permissions.push({
          tool: e.tool,
          toolUseId: e.tool_use_id,
          decision: verdict.decision,
          reason: verdict.reason,
          rule: verdict.rule,
          at: await $.clock.now(),
          outcome: verdict.decision === 'deny' ? 'denied' : undefined,
        })
        turn.permissions = turn.permissions.slice(-MAX_PER_TURN)
        await save($, s)
      }
    } catch {
      // the verdict stands as the engine gave it
    }
    return verdict
  }).catch(($, e, next) => next(e))

  on('tool.call', async ($, e, next) => {
    const rec = await ensure($, s)
    const turn = rec === null ? undefined : current(rec)
    if (turn === undefined) {
      const ran = await next(e)
      await endCall($, s, e.tool)
      return ran
    }

    try {
      turn.tools[e.tool] = (turn.tools[e.tool] ?? 0) + 1
      // a webhook posted from a shell: Bash, or PowerShell on Windows
      const command = e.tool === 'Bash' || e.tool === 'PowerShell' ? e.command : undefined
      // a file sent to reach the person's phone pings them, as PushNotification does
      const proactive = e.tool === 'SendUserFile' && e.status === 'proactive'
      if (PING_TOOL.test(e.tool) || proactive || (command !== undefined && WEBHOOK.test(command))) {
        turn.pings.push({ via: e.tool, detail: cut(JSON.stringify(e), 0, 200), at: await $.clock.now() })
        turn.pings = turn.pings.slice(-MAX_PER_TURN)
      }
    } catch {
      // count what we can
    }

    const ran = await next(e)
    await endCall($, s, e.tool)

    try {
      const asked =
        e.tool_use_id === undefined ? undefined : turn.permissions.find(p => p.toolUseId === e.tool_use_id)
      const text = ran.deny ?? (ran.isError === true ? String(ran.text ?? '') : '')
      const isDenied = ran.deny !== undefined || (ran.isError === true && DENIED_TEXT.test(text))
      if (ran.isError === true || ran.deny !== undefined) {
        turn.toolErrors++
        turn.lastError = `${e.tool}: ${cut(text, 0, 200)}`
      }
      if (asked !== undefined) {
        asked.outcome = UNANSWERED_TEXT.test(text)
          ? 'unanswered'
          : isDenied
            ? 'denied'
            : ran.isError === true
              ? 'error'
              : 'allowed'
        if (text !== '') asked.outcomeText = cut(text, 0, 200)
      } else if (isDenied || UNANSWERED_TEXT.test(text)) {
        turn.permissions.push({
          tool: e.tool,
          toolUseId: e.tool_use_id,
          decision: isDenied ? 'deny' : 'ask',
          at: await $.clock.now(),
          outcome: isDenied ? 'denied' : 'unanswered',
          outcomeText: cut(text, 0, 200),
        })
      }
      await save($, s)
    } catch {
      // the tool's result goes back untouched
    }
    return ran
  }).catch(($, e, next) => next(e))

  on('turn.complete', async ($, e, next) => {
    const done = await next(e)
    if (e.agentId !== undefined) return done
    await clearPending($, s)
    try {
      const rec = await ensure($, s)
      const turn = rec?.turns.find(t => t.id === e.turnId)
      if (rec !== null && turn !== undefined) {
        const endedAt = await $.clock.now()
        turn.endedAt = endedAt
        turn.end = e.reason
        turn.answerTail = cut(done.text.trim(), -TAIL) || undefined
        const last = [...done.text.matchAll(STATUS_LINE)].at(-1)
        if (last !== undefined) {
          // the groups of the form that matched
          turn.status = (last[1] ?? last[3] ?? last[5])?.toLowerCase() as RunStatus
          turn.statusText = (last[2] ?? last[4] ?? last[6])?.trim() || undefined
        }
        for (const ping of done.text.matchAll(PING_LINE)) {
          const detail = ping[1] ?? ping[2] ?? ping[3] ?? ''
          turn.pings.push({ via: 'PING line', detail: cut(detail, 0, 200), at: endedAt })
        }
        if (rec.activeTurnId === e.turnId) rec.activeTurnId = null
        await save($, s)
      }
    } catch {
      // the answer stands
    }
    return done
  })
}

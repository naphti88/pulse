import { expect, mock, test } from 'claude-code/testing'
import type { On } from 'claude-code'

import { statusPrompt, withoutStatus, type RunRecord } from '../hooks/register'

const FILE = '/home/t/.claude/routine-recorder/runs/s1.json'

function world(on: On, answer: string) {
  const files = new Map<string, string>()
  mock.env(on, { HOME: '/home/t' })
  mock.clock(on, { now: 1000 })
  on('session.id', () => ({ value: 's1' }))
  on('session.cwd', () => ({ value: '/work' }))
  on('fs.exists', ($, e) => ({ value: files.has(e.path) }))
  on('fs.read', ($, e) => ({ value: files.get(e.path) ?? '' }))
  on('fs.write', ($, e) => {
    files.set(e.path, e.text)
    return { value: undefined }
  })
  const contexts: (readonly string[] | undefined)[] = []
  on('prompt.submit', ($, e) => {
    contexts.push(e.context)
    return { text: e.text }
  })
  on('turn.start', ($, e) => ({ turnId: e.turnId }))
  on('tool.check', () => ({ decision: 'ask' as const, reason: 'Bash needs approval' }))
  on('tool.call', () => ({ deny: "The user doesn't want to proceed with this tool use." }))
  on('turn.complete', () => ({ text: answer }))
  return { files, contexts }
}

const PENDING = '/home/t/.claude/routine-recorder/pending/s1.json'
const pendingOf = (files: Map<string, string>) => JSON.parse(files.get(PENDING) ?? 'null')

test('what stands before the person is told while it lasts, by its name only', async ($, on) => {
  const { files } = world(on, 'Fertig.\nSTATUS: worked | Fertig.')
  on('classic.PermissionRequest', () => ({}))
  on('classic.Elicitation', () => ({}))
  on('classic.ElicitationResult', () => ({}))

  await $.prompt.submit({ text: 'Mach das', wait: false, origin: { kind: 'user' } })
  await $.turn.start({ text: '', turnId: 't1' })
  await $.classic.PermissionRequest({ tool_name: 'mcp__mail__send', tool_input: { to: 'secret' } })
  expect(pendingOf(files)).toEqual([{ tool: 'mcp__mail__send', at: 1000 }])
  expect(files.get(PENDING)).not.toContain('secret')

  // a call beside it ends: the request still stands
  await $.tool.call({ tool: 'mcp__plan__update', tool_use_id: 'u0' })
  expect(pendingOf(files)).toEqual([{ tool: 'mcp__mail__send', at: 1000 }])

  // its own call goes on: nothing stands before the person any more
  await $.tool.call({ tool: 'mcp__mail__send', tool_use_id: 'u1' })
  expect(pendingOf(files)).toEqual([])

  await $.classic.Elicitation({ mcp_server_name: 'mail', message: 'secret question' })
  expect(pendingOf(files)).toEqual([{ tool: 'mcp__', asks: true, at: 1000 }])
  expect(files.get(PENDING)).not.toContain('secret')
  await $.classic.ElicitationResult({ mcp_server_name: 'mail' })
  expect(pendingOf(files)).toEqual([])

  await $.classic.PermissionRequest({ tool_name: 'Bash', agent_id: 'a1' })
  expect(pendingOf(files)).toEqual([{ tool: 'Bash', agent: 'a1', at: 1000 }])
  await $.turn.complete({ answer: '', durationMs: 5, isAborted: false, turnId: 't1', reason: 'answer' })
  expect(pendingOf(files)).toEqual([])
})

const read = (files: Map<string, string>) => JSON.parse(files.get(FILE) ?? 'null') as RunRecord | null

test('a routine run is recorded with its denial, ping and STATUS line', async ($, on) => {
  const { files, contexts } = world(on, 'Konnte nicht.\nPING: Discord an das Team\nSTATUS: blocked | SSH-Tunnel abgelehnt.')

  await $.prompt.submit({
    text: '<scheduled-task name="weekly-report" file="/x/SKILL.md">\nRun it.',
    wait: false,
    origin: { kind: 'scheduled-trigger' },
  })
  await $.turn.start({ text: '', turnId: 't1' })
  await $.tool.check({ tool: 'Bash', input: { command: 'ssh host' }, tool_use_id: 'u1' })
  await $.tool.call({ tool: 'Bash', command: 'ssh host', tool_use_id: 'u1' })
  await $.turn.complete({ answer: '', durationMs: 5, isAborted: false, turnId: 't1', reason: 'answer' })

  const rec = read(files)
  expect(rec?.taskId).toBe('weekly-report')
  expect(rec?.activeTurnId).toBe(null)
  const turn = rec?.turns[0]
  expect(rec?.source).toBe('live')
  expect(turn?.by).toBe('routine')
  expect(turn?.trigger).toBe('scheduled')
  expect(turn?.answerTail).toContain('STATUS: blocked')
  expect(turn?.status).toBe('blocked')
  expect(turn?.statusText).toBe('SSH-Tunnel abgelehnt.')
  expect(turn?.tools['Bash']).toBe(1)
  expect(turn?.permissions[0]?.decision).toBe('ask')
  expect(turn?.permissions[0]?.outcome).toBe('denied')
  expect(turn?.pings[0]?.detail).toBe('Discord an das Team')

  expect(contexts[0]?.at(-1)).toContain('\nSTATUS: <idle|worked|waiting|blocked|parked>')
  expect(contexts[0]?.at(-1)).toContain('scheduled routines')
})

test('the person continuing in the run is recorded as a user turn', async ($, on) => {
  const { files, contexts } = world(on, 'Erledigt.\n\n[//]: # (PING: Push an mich)\n[//]: # (STATUS: waiting | Zwei Punkte noch offen.)')

  await $.prompt.submit({ text: '<scheduled-task name="ops">\nGo', wait: false, origin: { kind: 'scheduled-trigger' } })
  await $.turn.start({ text: '', turnId: 't1' })
  await $.turn.complete({ answer: '', durationMs: 5, isAborted: false, turnId: 't1', reason: 'answer' })
  await $.prompt.submit({ text: 'Ja, lade es.', wait: false, origin: { kind: 'composer' } })
  await $.turn.start({ text: 'Ja, lade es.', turnId: 't2' })
  await $.turn.complete({ answer: '', durationMs: 5, isAborted: false, turnId: 't2', reason: 'answer' })

  const rec = read(files)
  expect(rec?.turns.map(t => t.by)).toEqual(['routine', 'user'])
  expect(rec?.turns[1]?.trigger).toBe(undefined)
  expect(rec?.turns[1]?.status).toBe('waiting')
  expect(rec?.turns[1]?.statusText).toBe('Zwei Punkte noch offen.')
  expect(rec?.turns[1]?.pings[0]?.detail).toBe('Push an mich')
  expect(contexts[1]?.at(-1)).toContain('STATUS:')
})

test('an ordinary chat is not recorded but asked for its STATUS', async ($, on) => {
  const { files, contexts } = world(on, 'Hallo.')

  await $.prompt.submit({ text: 'Hallo', wait: false, origin: { kind: 'composer' } })
  await $.turn.start({ text: 'Hallo', turnId: 't1' })
  await $.turn.complete({ answer: '', durationMs: 5, isAborted: false, turnId: 't1', reason: 'answer' })

  expect(files.size).toBe(0)
  expect(contexts[0]?.at(-1)).toContain('\nSTATUS: <idle|worked|waiting|blocked|parked>')
  expect(contexts[0]?.at(-1)).not.toContain('scheduled routines')
  expect(contexts[0]?.at(-1)).not.toContain('PING')
})

test('a command the person runs from the chat is a round of theirs', async ($, on) => {
  const { files } = world(on, 'Bitte führ den Befehl selbst aus.\nSTATUS: blocked | Prüfung abgelehnt.')

  await $.prompt.submit({ text: '<scheduled-task name="ops">\nGo', wait: false, origin: { kind: 'scheduled-trigger' } })
  await $.turn.start({ text: '', turnId: 't1' })
  await $.tool.check({ tool: 'Bash', input: { command: 'check' }, tool_use_id: 'u1' })
  await $.tool.call({ tool: 'Bash', command: 'check', tool_use_id: 'u1' })
  await $.turn.complete({ answer: '', durationMs: 5, isAborted: false, turnId: 't1', reason: 'answer' })
  await $.prompt.submit({ text: '<bash-input>check</bash-input><bash-stdout>OK</bash-stdout>', wait: false, origin: { kind: 'composer' } })

  const rec = read(files)
  expect(rec?.turns.map(t => t.by)).toEqual(['routine', 'user'])
  expect(rec?.turns.at(-1)?.permissions).toEqual([])
})

test('a parked topic is recorded with what comes next', async ($, on) => {
  const { files } = world(on, 'Stand: Analyse halb fertig.\n\nSTATUS: parked | RAM-Messung steht, nächster Schritt: Profil des Workers.')

  await $.prompt.submit({ text: '<scheduled-task name="ops">\nGo', wait: false, origin: { kind: 'scheduled-trigger' } })
  await $.turn.start({ text: '', turnId: 't1' })
  await $.turn.complete({ answer: '', durationMs: 5, isAborted: false, turnId: 't1', reason: 'answer' })

  const turn = read(files)?.turns[0]
  expect(turn?.status).toBe('parked')
  expect(turn?.statusText).toBe('RAM-Messung steht, nächster Schritt: Profil des Workers.')
})

const FAILED = '/home/t/.claude/routine-recorder/failed/s1.json'

test('an API error that ends a turn is told by its kind until the next turn', async ($, on) => {
  const { files } = world(on, 'Fertig.\nSTATUS: worked | Fertig.')
  on('classic.StopFailure', () => ({}))

  await $.turn.start({ text: '', turnId: 't1' })
  expect(files.has(FAILED)).toBe(false)
  await $.classic.StopFailure({ error: 'rate_limit', error_details: 'secret', last_assistant_message: 'secret' })
  expect(JSON.parse(files.get(FAILED) ?? 'null')).toEqual({ error: 'rate_limit', at: 1000 })
  expect(files.get(FAILED)).not.toContain('secret')

  await $.classic.StopFailure({ error: 'overloaded', agent_id: 'a1' })
  expect(JSON.parse(files.get(FAILED) ?? 'null')).toEqual({ error: 'rate_limit', at: 1000 })

  await $.turn.start({ text: '', turnId: 't2' })
  expect(JSON.parse(files.get(FAILED) ?? '{}')).toBe(null)
})

test('what is shown of an answer leaves out the STATUS line it ends with', async ($, on) => {
  expect(withoutStatus('Fertig.\n\nSTATUS: worked | Alles da.')).toBe('Fertig.')
  expect(withoutStatus('Fertig.\n\nSTATUS: waiting | Eine Frage offen.\n')).toBe('Fertig.')
  expect(withoutStatus('STATUS: idle | Nichts zu tun.')).toBe('')
  expect(withoutStatus('Die Zeile `STATUS: worked` steht im Satz.')).toBe('Die Zeile `STATUS: worked` steht im Satz.')
  const quoted = 'So sieht es aus:\nSTATUS: worked | Beispiel\n\nDanach geht es weiter.'
  expect(withoutStatus(quoted)).toBe(quoted)

  on('classic.MessageDisplay', () => ({}))
  const end = { turn_id: 't1', message_id: 'm1', index: 0, final: true }
  expect(await $.classic.MessageDisplay({ ...end, delta: 'Fertig.\n\nSTATUS: worked | Alles da.' })).toEqual({ displayContent: 'Fertig.' })
  expect(await $.classic.MessageDisplay({ ...end, delta: 'Ohne Zeile.' })).toEqual({})
  expect(await $.classic.MessageDisplay({ ...end, final: false, delta: 'STATUS: worked | Mitten drin' })).toEqual({})
})

test('a call the engine sends to be asked for is told, for the app to ask in its own dialog', async ($, on) => {
  const { files } = world(on, 'Fertig.\nSTATUS: worked | Fertig.')

  await $.prompt.submit({ text: 'Mach das', wait: false, origin: { kind: 'user' } })
  await $.turn.start({ text: '', turnId: 't1' })
  await $.tool.check({ tool: 'mcp__Claude_Browser__preview_start', input: { name: 'secret' }, tool_use_id: 'u1' })
  expect(pendingOf(files)).toEqual([{ tool: 'mcp__Claude_Browser__preview_start', checked: true, at: 1000 }])
  expect(files.get(PENDING)).not.toContain('secret')

  await $.tool.call({ tool: 'mcp__Claude_Browser__preview_start', tool_use_id: 'u1' })
  expect(pendingOf(files)).toEqual([])
})

test('an answer that needs the person brings a card that stands on its own', async () => {
  for (const isRoutine of [true, false]) {
    const prompt = statusPrompt(isRoutine)
    expect(prompt).toContain('decision card before the STATUS line')
    for (const field of ['What it is about', 'To decide', 'Proposal', 'If nothing happens', 'Urgency', 'Importance', 'Connected with']) {
      expect(prompt).toContain(`- ${field}:`)
    }
  }
})

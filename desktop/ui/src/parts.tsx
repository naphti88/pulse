import { Fragment, useEffect, useState } from 'react'
import type { ReactNode } from 'react'

import { mark, markCommand, openChat, saveCase } from './api'
import { ageLevel, ago, clock, elapsed, since } from './format'
import { Disc, Glyph, STATE_LABEL, markOf, typeOf } from './glyphs'
import type { Mark, Type } from './glyphs'
import type { Check, Flag, Item, ParkedView } from './types'

// the time, ticking each second while the panel is shown
export function useNow() {
  const [now, setNow] = useState(Date.now())
  useEffect(() => {
    const timer = setInterval(() => {
      if (!document.hidden) setNow(Date.now())
    }, 1000)
    return () => clearInterval(timer)
  }, [])
  return now
}

// open or closed, kept across restarts of the panel
export function useOpen(key: string, initial: boolean): [boolean, () => void] {
  const stored = (() => {
    try {
      return localStorage.getItem(`open:${key}`)
    } catch {
      return null
    }
  })()
  const [open, setOpen] = useState(stored === null ? initial : stored === '1')
  const toggle = () =>
    setOpen(o => {
      try {
        localStorage.setItem(`open:${key}`, o ? '0' : '1')
      } catch {
        // without storage it stays as it is for this run
      }
      return !o
    })
  return [open, toggle]
}

// a group on its own card: the mark and name in the colour of the group, its
// count; a click on the head folds it, unless it is always open
export function Card(props: { id: string; mark?: Mark; title: ReactNode; count?: ReactNode; aside?: ReactNode; initial?: boolean; fixed?: boolean; className?: string; children: ReactNode }) {
  const [stored, toggle] = useOpen(props.id, props.initial ?? true)
  const open = props.fixed || stored
  return (
    <section className={`card ${props.mark ? `m-${props.mark}` : ''} ${props.className ?? ''}`}>
      <button className={`ch ${props.fixed ? 'fixed' : ''}`} onClick={props.fixed ? undefined : toggle}>
        {props.mark && <Glyph name={props.mark === 'quiet' ? 'done' : props.mark} size={14} className={props.mark === 'running' ? 'spin' : ''} />}
        <span className="ch-title">{props.title}</span>
        {props.aside && open ? <span className="ch-aside" onClick={e => e.stopPropagation()}>{props.aside}</span> : null}
        <span className="n">{props.count}</span>
        {!props.fixed && <span className={`chev ${open ? 'open' : ''}`}>▶</span>}
      </button>
      {open && <div className="card-body">{props.children}</div>}
    </section>
  )
}

export function Age({ t, now }: { t: number; now: number }) {
  return <span className={`age ${ageLevel(t, now)}`}>{ago(t, now)}</span>
}

function Action({ glyph, label, onClick }: { glyph: Parameters<typeof Glyph>[0]['name']; label: string; onClick: () => void }) {
  return (
    <button
      className="act"
      title={label}
      onClick={e => {
        e.stopPropagation()
        onClick()
      }}
    >
      <Glyph name={glyph} size={13} />
    </button>
  )
}

// where an item plays: its folder, and the session it came from when Claude started it
export function Where({ project, from, children }: { project: string; from?: string; children?: ReactNode }) {
  return (
    <span className="where">
      {project && (
        <span className="folder">
          <Glyph name="folder" size={12} />
          {project}
        </span>
      )}
      {from && <span className="from" title={`Started from “${from}”`}>from “{from}”</span>}
      {children}
    </span>
  )
}

// the title of the session an item came from, while the panel still has it
export const fromOf = (item: { parentId?: string }, items: Item[]) => (item.parentId ? items.find(i => i.id === item.parentId)?.title : undefined)

// why an item is what it is, as words to read or to hand on: the rules that
// decided, those before them that did not hold, and what was known. A case
// keeps none of what was said (src-tauri/src/scrub.rs) and stays on this Mac
function WhyBox({ item }: { item: Item }) {
  const { why } = item
  const [note, setNote] = useState('')
  const copy = () => {
    const lines = [
      `${item.title}: ${item.state}, ${item.stage}`,
      `Rule: ${why.rule}. ${why.says}`,
      `Stage: ${why.stageRule}. ${why.stageSays}`,
      ...why.facts,
      ...(why.passed.length > 0 ? [`Did not hold: ${why.passed.join(', ')}`] : []),
    ]
    navigator.clipboard.writeText(lines.join('\n')).then(() => setNote('Copied'), () => setNote('Could not copy'))
  }
  const save = () => {
    saveCase(item.id).then(path => setNote(`Saved on this Mac: ${path.split('/').pop()}`), e => setNote(String(e)))
  }
  return (
    <div className="why" onClick={e => e.stopPropagation()}>
      <p>
        <b>{why.rule}</b> {why.says}
      </p>
      <p>
        <b>{why.stageRule}</b> {why.stageSays}
      </p>
      {why.facts.map(fact => (
        <span key={fact} className="fact">
          {fact}
        </span>
      ))}
      {why.passed.length > 0 && <span className="fact passed">Did not hold: {why.passed.join(', ')}</span>}
      <span className="why-acts">
        <button className="link" onClick={copy}>
          Copy
        </button>
        <button className="link" title="Keeps the shape of this turn and nothing that was said" onClick={save}>
          Save case
        </button>
        <span className="note">{note}</span>
      </span>
    </div>
  )
}

// an item as a row: what it wants and what it is, then title, folder and what
// Claude said; a click opens its chat in Claude, on hover the marks take the
// place of the age
export function Row({ item, now, from }: { item: Item; now: number; from?: string }) {
  const isDone = item.stage === 'done'
  const [why, setWhy] = useState(false)
  return (
    <>
    <div className={`row ${isDone ? 'dim' : ''}`} title="Open in Claude" onClick={() => void openChat(item.id)}>
      <Disc mark={markOf(item)} type={typeOf(item)} alert={item.state === 'blocked' && item.stage === 'open'} />
      <b className="title" title={item.title}>
        {item.title}
      </b>
      <span className="right">
        {item.stage === 'running' ? <span className="age run">running</span> : isDone ? <span className="age">{clock(item.since)}</span> : <Age t={item.since} now={now} />}
        <span className="acts">
          {item.stage === 'open' && item.state !== 'review' && <Action glyph="follow" label="Later: read, stays open" onClick={() => void mark([item.id], 'seen', item.since)} />}
          {(item.stage === 'open' || item.stage === 'follow') && <Action glyph="check" label="Done" onClick={() => void mark([item.id], 'done', item.since)} />}
          {item.stage === 'parked' ? (
            <Action glyph="unpark" label="Unpark" onClick={() => void mark([item.id], 'resume')} />
          ) : (
            item.stage !== 'running' && <Action glyph="parked" label="Park: keep it in mind" onClick={() => void mark([item.id], 'park')} />
          )}
          <Action glyph="why" label="Why is it here?" onClick={() => setWhy(w => !w)} />
        </span>
      </span>
      <Where project={item.project} from={from} />
      {item.stage !== 'running' && item.reason && <p title={item.reason}>{item.reason}</p>}
    </div>
    {why && <WhyBox item={item} />}
    </>
  )
}

const SHOWN = 3

// Running now: what runs right now, above every tab and the one place that changes
// by the second; each with what it does now and a line per subagent at work;
// the first three, the rest on request, in the order they began
export function RunCard({ items, now }: { items: Item[]; now: number }) {
  const [all, setAll] = useState(false)
  const [why, setWhy] = useState<string | null>(null)
  if (items.length === 0) return null
  const sorted = items.slice().sort((a, b) => a.startedAt - b.startedAt || a.id.localeCompare(b.id))
  return (
    <Card id="running" mark="running" title="Running now" count={items.length} className="run">
      {(all ? sorted : sorted.slice(0, SHOWN)).map(i => {
        const agents = i.subagents.filter(a => a.isRunning).sort((a, b) => a.startedAt - b.startedAt)
        return (
          <Fragment key={i.id}>
          <div className="row" title={`${i.title} · ${i.project}\nOpen in Claude`} onClick={() => void openChat(i.id)}>
            <Disc mark="running" type={typeOf(i)} />
            <b className="title">{i.title}</b>
            <span className="right">
              <span className="tm">{elapsed(i.live?.turnStarted ?? i.since, now)}</span>
              <span className="acts">
                <Action glyph="why" label="Why is it here?" onClick={() => setWhy(id => (id === i.id ? null : i.id))} />
              </span>
            </span>
            <Where project={i.project} from={fromOf(i, items)} />
            <span className="doing">▸ {i.live?.tool ?? 'thinking'}</span>
            {agents.map(a => (
              <span key={a.id} className="agent" style={{ marginLeft: Math.max(0, a.depth - 1) * 14 }} title={`${a.type || 'Subagent'}${a.isBackground ? ' in the background' : ''}\n${a.description}`}>
                <Glyph name="agent" size={13} />
                <span className="what">
                  <b>{a.type || 'Subagent'}</b>
                  {a.description && ` · ${a.description}`}
                </span>
                <span className="tm">{elapsed(a.startedAt, now)}</span>
              </span>
            ))}
          </div>
          {why === i.id && <WhyBox item={i} />}
          </Fragment>
        )
      })}
      {sorted.length > SHOWN && (
        <button className="link more" onClick={() => setAll(a => !a)}>
          {all ? 'less' : `+${sorted.length - SHOWN} more`}
        </button>
      )}
    </Card>
  )
}

export const stateWord = (item: Item) => (item.state !== 'quiet' && item.stage === 'done' ? 'done' : STATE_LABEL[item.state])

export const FLAG_LABEL: Record<Flag, string> = {
  hot: 'running hot',
  stuck: 'stuck',
  forgotten: 'forgotten?',
  long: 'running long',
}

const FLAG_TITLE: Record<Flag, string> = {
  hot: 'Has used almost a whole core for a long time',
  stuck: 'The session has been waiting for this command for a long time',
  forgotten: 'Runs in the background, its session is quiet',
  long: 'Has been running in the background for a long time',
}

const megabytes = (bytes: number) => (bytes >= 1024 ** 3 ? `${(bytes / 1024 ** 3).toFixed(1)} GB` : `${Math.round(bytes / 1024 ** 2)} MB`)

// a command worth a look: what it is and why, where it runs, what it uses
export function CheckRow({ check, now }: { check: Check; now: number }) {
  const uses = [
    `running ${since(check.started, now)}`,
    check.cpu >= 0.05 ? `${Math.round(check.cpu * 100)} % CPU` : '',
    megabytes(check.memory),
    ...check.ports.map(p => `:${p}`),
    check.background ? `output ${ago(check.lastOutput, now)}` : 'in the foreground',
  ].filter(Boolean)
  return (
    <div className={`row check ${check.known ? 'dim' : ''}`} title={check.sessionId ? 'Open in Claude' : undefined} onClick={() => check.sessionId && void openChat(check.sessionId)}>
      <Disc mark="commands" />
      <b className="title" title={check.what}>
        {check.what}
      </b>
      <span className="right">
        {check.flag && (
          <span className={`flag ${check.known ? '' : check.flag}`} title={FLAG_TITLE[check.flag]}>
            {check.known ? 'known' : FLAG_LABEL[check.flag]}
          </span>
        )}
        <span className="acts">
          {check.known ? (
            <Action glyph="eye" label="Check again" onClick={() => void markCommand(check.key, false)} />
          ) : (
            <Action glyph="check" label="Known: fine for now, report again at the next reminder step" onClick={() => void markCommand(check.key, true)} />
          )}
        </span>
      </span>
      <Where project={check.project}>
        <span className="from" title={check.title}>
          {check.title}
        </span>
      </Where>
      <p title={uses.join(' · ')}>{uses.join(' · ')}</p>
    </div>
  )
}

// a topic set aside: what it is, where it stands, since when; a click opens it
// in Claude to look, and it stays parked until it is unparked or goes on; a
// note whose session is gone can only be dropped
export function ParkedRow({ parked: p, now, type }: { parked: ParkedView; now: number; type?: Type }) {
  return (
    <div className={`row ${p.gone ? 'dim' : ''}`} title={p.gone ? 'The app no longer has this session' : 'Open in Claude'} onClick={() => !p.gone && void openChat(p.id)}>
      <Disc mark="parked" type={type} />
      <b className="title" title={p.title}>
        {p.title}
      </b>
      <span className="right">
        <span className="age" title="parked">
          {ago(p.at, now)}
        </span>
        <span className="acts">
          {p.gone ? (
            <Action glyph="close" label="Remove note" onClick={() => void mark([p.id], 'forget')} />
          ) : (
            <Action glyph="unpark" label="Unpark" onClick={() => void mark([p.id], 'resume')} />
          )}
        </span>
      </span>
      <Where project={p.project}>{p.gone && <span className="from">session gone</span>}</Where>
      {p.said && <p title={p.said}>{p.said}</p>}
    </div>
  )
}

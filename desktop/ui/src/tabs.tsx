import { mark, openChat } from './api'
import { DAY, clock, dayName, dayOf, until } from './format'
import { Disc, Glyph, TYPE_LABEL, markOf, typeOf } from './glyphs'
import { appOf, Card, CheckRow, ParkedRow, Row, fromOf, stateWord } from './parts'
import type { Item, Snapshot } from './types'

// Open: what needs the person, one card per state, the most urgent first;
// in each what lies longer than a day on top, the oldest first; below, what
// they saw and have not closed, what they set aside and the commands marked
// as known, each on a folded card of its own below a rule
export function OpenTab({ snap, now }: { snap: Snapshot; now: number }) {
  const open = snap.items.filter(i => i.stage === 'open')
  const follow = snap.items.filter(i => i.stage === 'follow')
  // the commands worth a look; those marked as known only folded, until their next step
  const checks = snap.checks.filter(c => c.flag && !c.known)
  const known = snap.checks.filter(c => c.flag && c.known)
  const ordered = (list: Item[]) => [...list.filter(i => now - i.since >= DAY).sort((a, b) => a.since - b.since), ...list.filter(i => now - i.since < DAY)]
  const groups = [
    ['blocked', 'Blocked'],
    ['waiting', 'Question'],
    ['review', 'Result to review'],
  ] as const
  const row = (i: Item) => <Row key={i.id} item={i} now={now} from={fromOf(i, snap.items)} />
  return (
    <>
      {checks.length > 0 && (
        <Card id="open-checks" mark="commands" title="Commands to check" count={checks.length}>
          {checks.map(c => (
            <CheckRow key={c.key} check={c} now={now} />
          ))}
        </Card>
      )}
      {open.length === 0 && (
        <div className="empty">
          <Glyph name="done" size={40} className="s-calm" />
          <b>Nothing open</b>
          <span>Everything answered and reviewed.</span>
        </div>
      )}
      {groups.map(([state, title]) => {
        const list = open.filter(i => i.state === state)
        return (
          list.length > 0 && (
            <Card key={state} id={`open-${state}`} mark={state} title={title} count={list.length}>
              {ordered(list).map(row)}
            </Card>
          )
        )
      })}
      {(follow.length > 0 || snap.parked.length > 0 || known.length > 0) && (
        <div className="set-aside">
          <span>Set aside</span>
        </div>
      )}
      {follow.length > 0 && (
        <Card
          id="open-follow"
          initial={false}
          mark="follow"
          title="Later · read, still open"
          count={follow.length}
          className="aside"
          aside={
            <button className="link" onClick={() => void mark(follow.map(i => i.id), 'done')}>
              All done
            </button>
          }
        >
          {follow.map(row)}
        </Card>
      )}
      {snap.parked.length > 0 && (
        <Card id="open-parked" initial={false} mark="parked" title="Parked · kept in mind" count={snap.parked.length} className="aside">
          {snap.parked.map(p => {
            const item = snap.items.find(i => i.id === p.id)
            return <ParkedRow key={p.id} parked={p} now={now} type={item && typeOf(item)} />
          })}
        </Card>
      )}
      {known.length > 0 && (
        <Card id="open-known" initial={false} mark="commands" title="Known · until the next step" count={known.length} className="aside">
          {known.map(c => (
            <CheckRow key={c.key} check={c} now={now} />
          ))}
        </Card>
      )}
    </>
  )
}

type Event = { key: string; at: number; item?: Item; title: string; kind: Item['kind']; note: string; count: number }

// Timeline: today until now, what runs, then what comes; runs of the same
// routine that found nothing in a row are one line
export function TimelineTab({ snap, now }: { snap: Snapshot; now: number }) {
  const today = snap.items.filter(i => dayOf(i.startedAt) === dayOf(now) && i.stage !== 'running').sort((a, b) => a.startedAt - b.startedAt)
  const past: Event[] = []
  for (const i of today) {
    const prev = past.at(-1)
    if (prev?.item && i.state === 'quiet' && prev.item.state === 'quiet' && prev.title === i.title) {
      prev.count += 1
      continue
    }
    past.push({ key: i.id, at: i.startedAt, item: i, title: i.title, kind: i.kind, note: stateWord(i), count: 1 })
  }
  const running = snap.items.filter(i => i.stage === 'running')
  const coming = snap.planned.filter(p => p.at < now + 2 * DAY)
  const days = [...new Set(coming.map(p => dayOf(p.at)))]
  const type = (i: { kind: Item['kind']; parentId?: string }) => {
    const t = typeOf(i)
    return <Glyph name={t} size={13} className="type" title={TYPE_LABEL[t]} />
  }
  return (
    <section className="card tl">
      <div className="day">Today</div>
      {past.map(e => (
        <div className={`ev open-chat ${e.item?.state === 'quiet' ? 'dim' : ''}`} key={e.key} title={`Open in ${e.item ? appOf(e.item.id) : "Claude"}`} onClick={() => e.item && void openChat(e.item.id)}>
          <span className="h">{clock(e.at)}</span>
          {e.item && <Disc small mark={markOf(e.item)} />}
          <span className="t1">
            {e.item ? type(e.item) : type(e)}
            <b dir="auto">
              {e.title}
              {e.count > 1 ? ` ×${e.count}` : ''}
            </b>
          </span>
          <span className="r">{e.note}</span>
        </div>
      ))}
      {running.map(i => (
        <div className="ev open-chat" key={i.id} title={`Open in ${appOf(i.id)}`} onClick={() => void openChat(i.id)}>
          <span className="h">{clock(i.startedAt)}</span>
          <Disc small mark="running" />
          <span className="t1">
            {type(i)}
            <b dir="auto">{i.title}</b>
          </span>
          <span className="r">running</span>
        </div>
      ))}
      <div className="now">
        <span>{clock(now)}</span>
      </div>
      {days.map(d => (
        <div key={d}>
          {d !== dayOf(now) && <div className="day">{dayName(coming.find(p => dayOf(p.at) === d)?.at ?? now, now)}</div>}
          {coming
            .filter(p => dayOf(p.at) === d)
            .map(p => (
              <div className="ev future" key={`${p.title}${p.at}`}>
                <span className="h">{clock(p.at)}</span>
                <Glyph name="planned" size={15} className="s-muted" />
                <span className="t1">
                  {type(p)}
                  <b dir="auto">{p.title}</b>
                </span>
                <span className="r">{p.count > 1 ? `×${p.count} until ${clock(p.last)}` : d === dayOf(now) ? until(p.at, now) : p.kind === 'oneoff' ? 'once' : ''}</span>
              </div>
            ))}
        </div>
      ))}
      {snap.missed.length > 0 && (
        <>
          <div className="day">Missed</div>
          {snap.missed.slice(0, 8).map(m => (
            <div className="ev" key={`${m.title}${m.at}`}>
              <span className="h">{clock(m.at)}</span>
              <Glyph name="missed" size={15} className="s-muted" />
              <span className="t1">
                {type({ kind: 'recurring' })}
                <b dir="auto">{m.title}</b>
              </span>
              <span className="r">{dayName(m.at, now)}</span>
            </div>
          ))}
        </>
      )}
    </section>
  )
}

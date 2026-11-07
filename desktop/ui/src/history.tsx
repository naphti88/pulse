import { useEffect, useMemo, useRef, useState } from 'react'

import { getHistory, mark, openChat } from './api'
import { clock, dayName, dayOf } from './format'
import { Disc, Glyph, markOf, typeOf } from './glyphs'
import { appOf, Card, Where } from './parts'
import type { Entry } from './types'

type Filter = 'chat' | 'routine' | 'all'

const FILTERS: [Filter, string][] = [
  ['chat', 'Sessions'],
  ['routine', 'Routines'],
  ['all', 'All'],
]

function useStoredFilter(): [Filter, (f: Filter) => void] {
  const [filter, setFilter] = useState<Filter>(() => {
    try {
      return (localStorage.getItem('history-filter') as Filter | null) ?? 'chat'
    } catch {
      return 'chat'
    }
  })
  return [
    filter,
    f => {
      setFilter(f)
      try {
        localStorage.setItem('history-filter', f)
      } catch {
        // kept for this run only
      }
    },
  ]
}

// a run of the same routine on the same day, told once with its count
type Line = { entry: Entry; count: number }

// History: every session the app keeps, archived ones too, by its last
// activity; a search through title, folder and what it said last; a click
// goes back into it. Loaded when the tab opens, again when the panel's data
// changes.
export function HistoryTab({ version, now }: { version: number; now: number }) {
  const [entries, setEntries] = useState<Entry[] | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [query, setQuery] = useState('')
  const [filter, setFilter] = useStoredFilter()
  const search = useRef<HTMLInputElement>(null)

  useEffect(() => {
    getHistory().then(
      list => {
        setEntries(list)
        setError(null)
      },
      e => setError(String(e)),
    )
  }, [version])
  useEffect(() => search.current?.focus(), [])

  const days = useMemo(() => {
    const words = query.toLowerCase().split(/\s+/).filter(Boolean)
    const shown = (entries ?? []).filter(e => {
      if (filter === 'chat' && e.kind !== 'chat') return false
      if (filter === 'routine' && e.kind === 'chat') return false
      const text = `${e.title} ${e.project} ${e.said}`.toLowerCase()
      return words.every(w => text.includes(w))
    })
    const byDay: [number, Line[]][] = []
    for (const e of shown) {
      const day = dayOf(e.at)
      let group = byDay.at(-1)
      if (!group || group[0] !== day) {
        group = [day, []]
        byDay.push(group)
      }
      const same = e.kind !== 'chat' && group[1].find(l => l.entry.kind !== 'chat' && l.entry.title === e.title)
      if (same) same.count += 1
      else group[1].push({ entry: e, count: 1 })
    }
    return byDay
  }, [entries, query, filter])
  // the titles of the sessions others came from
  const parents = useMemo(() => new Map((entries ?? []).map(e => [e.id, e.title])), [entries])

  return (
    <div className="hist">
      <div className="hist-bar">
        <input ref={search} type="search" placeholder="Search: title, folder, last state" value={query} onChange={e => setQuery(e.target.value)} />
        <div className="seg small">
          {FILTERS.map(([id, label]) => (
            <button key={id} className={filter === id ? 'on' : ''} onClick={() => setFilter(id)}>
              {label}
            </button>
          ))}
        </div>
      </div>
      {error && <div className="empty error">{error}</div>}
      {!entries && !error && <div className="empty">loading …</div>}
      {entries && days.length === 0 && <div className="empty">Nothing found.</div>}
      {days.map(([day, lines]) => (
        <Card key={day} id={`hist-${day}`} fixed title={dayName(lines[0]!.entry.at, now)} count={lines.length}>
          {lines.map(({ entry: e, count }) => (
            <div key={e.id} className={`row ${e.archived ? 'dim' : ''}`} title={`${e.title}\n${e.said}\nOpen in ${appOf(e.id)}`} onClick={() => void openChat(e.id)}>
              <Disc mark={markOf(e)} type={typeOf(e)} />
              <b className="title" dir="auto">
                {e.title}
                {count > 1 ? ` ×${count}` : ''}
              </b>
              <span className="right">
                <span className="age">{clock(e.at)}</span>
                {e.stage !== 'running' && (
                  <span className="acts">
                    <button
                      className="act"
                      title={e.stage === 'parked' ? 'Unpark' : 'Park: keep it in mind'}
                      onClick={ev => {
                        ev.stopPropagation()
                        void mark([e.id], e.stage === 'parked' ? 'resume' : 'park')
                      }}
                    >
                      <Glyph name={e.stage === 'parked' ? 'unpark' : 'parked'} size={13} />
                    </button>
                  </span>
                )}
              </span>
              <Where project={e.project} from={parents.get(e.parentId ?? '')} id={e.id}>
                {(e.stage === 'parked' || e.archived || e.stage === 'running') && <span className="from">{e.stage === 'parked' ? 'parked' : e.stage === 'running' ? 'running' : 'archived'}</span>}
              </Where>
              {e.said && <p dir="auto">{e.said}</p>}
            </div>
          ))}
        </Card>
      ))}
    </div>
  )
}

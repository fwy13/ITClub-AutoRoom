import { useI18n, type Language } from './i18n'
import { useCallback, useEffect, useMemo, useState, type FormEvent, type ReactNode } from 'react'
import { ArrowRight, Bell, BookOpen, CalendarDays, CheckCircle2, Clock3, ExternalLink, LayoutGrid, Link2, LockKeyhole, LogOut, MapPin, RefreshCw, ShieldCheck, Users, X } from 'lucide-react'

type Status = 'available' | 'vacant' | 'reserved' | 'unavailable' | 'closed'
type Room = {id:number,name:string,floor:string}
type Slot = {room_id:number,date:string,slot:number,status:Status}
type Schedule = {rooms:Room[],slots:Slot[],synced_at:number|null,source:string|null,stale:boolean}
type User = {id:number,username:string,ctu_username:string,telegram_linked:boolean,telegram_username:string|null}
type Booking = {id:number,room_id:number,room_name:string,date:string,slot:number,students:number,note:string,status:'confirmed'|'failed'|'pending'|'uncertain',message:string,created_at:number}
type Job = {id:number,room_id:number,room_name:string,date:string,slot:number,students:number,note:string,borrow_tv:boolean,run_at:number,kind:'manual'|'prebook',opens_at:number|null,retry_until:number|null,attempts:number,recovery_count:number,recovered_at:number|null,execution_phase:string,status:string,message:string,booking_id:number|null}

async function api<T>(path:string, method='GET', body?:unknown):Promise<T> {
  const r=await fetch(path,{method,credentials:'include',headers:body===undefined?{}:{'Content-Type':'application/json'},body:body===undefined?undefined:JSON.stringify(body)})
  const text=await r.text()
  let data:unknown=null
  try { data=text?JSON.parse(text):null } catch { throw new Error('Invalid server response') }
  if(!r.ok) throw new Error((data as {error?:string}|null)?.error??`HTTP ${r.status}`)
  return data as T
}
const slotTimes:Record<number,string>={1:'07:30 – 09:30',2:'09:40 – 11:40',3:'11:50 – 13:50',4:'14:00 – 16:00',5:'16:10 – 18:10',6:'18:20 – 20:20'}
const states:Record<Status,{label:string,cls:string,dot:string}>={
  available:{label:'Bookable',cls:'bg-emerald-50 border-emerald-200 text-emerald-800 hover:bg-emerald-100',dot:'bg-emerald-500'},
  vacant:{label:'Empty (check CTU)',cls:'bg-blue-50 border-blue-200 text-blue-800 hover:bg-blue-100',dot:'bg-blue-500'},
  reserved:{label:'Reserved',cls:'bg-rose-50 border-rose-100 text-rose-500',dot:'bg-rose-400'},
  unavailable:{label:'Not open',cls:'bg-slate-50 border-slate-100 text-slate-400',dot:'bg-slate-300'},
  closed:{label:'Closed',cls:'bg-slate-100 border-slate-100 text-slate-400',dot:'bg-slate-400'}
}
// Calculate date arithmetic in UTC; display and scheduler timestamps use ICT (UTC+07).
const vnDatePlusDays=(days:number)=>{const now=new Date(Date.now()+7*3600_000);return new Date(Date.UTC(now.getUTCFullYear(),now.getUTCMonth(),now.getUTCDate()+days)).toISOString().slice(0,10)}
const minusDays=(date:string,days:number)=>{const d=new Date(`${date}T00:00:00Z`);d.setUTCDate(d.getUTCDate()-days);return d.toISOString().slice(0,10)}
const dateLabel=(d:string,language:Language)=>new Date(`${d}T12:00:00`).toLocaleDateString(language==='vi'?'vi-VN':'en-US',{weekday:'short',month:'short',day:'numeric'})
const fullDate=(d:string,language:Language)=>new Date(`${d}T12:00:00`).toLocaleDateString(language==='vi'?'vi-VN':'en-US',{weekday:'long',month:'long',day:'numeric',year:'numeric'})
const timeAgo=(t:number|null,language:Language)=>t===null?(language==='vi'?'Chưa đồng bộ':'Not synchronized'):new Date(t*1000).toLocaleString(language==='vi'?'vi-VN':'en-GB',{timeZone:'Asia/Ho_Chi_Minh',dateStyle:'medium',timeStyle:'short'})+' ICT'

function Notice({children,kind='warning'}:{children:ReactNode,kind?:'warning'|'error'|'success'}) {
  return <div className={`rounded-xl border px-4 py-3 text-sm ${kind==='error'?'bg-red-50 border-red-200 text-red-700':kind==='success'?'bg-green-50 border-green-200 text-green-800':'bg-amber-50 border-amber-200 text-amber-800'}`}>{children}</div>
}

function AuthForm({onDone,onClose}:{onDone:(u:User)=>void,onClose:()=>void}){
  const {t}=useI18n()
  const [register,setRegister]=useState(false)
  const [username,setUsername]=useState('')
  const [password,setPassword]=useState('')
  const [ctuUsername,setCtuUsername]=useState('')
  const [ctuPassword,setCtuPassword]=useState('')
  const [busy,setBusy]=useState(false)
  const [error,setError]=useState('')
  async function submit(e:FormEvent){
    e.preventDefault();setBusy(true);setError('')
    try {
      const u=await api<User>(register?'/api/auth/register':'/api/auth/login','POST',register?{username,password,ctu_username:ctuUsername,ctu_password:ctuPassword}:{username,password})
      onDone(u)
    } catch(e){setError(e instanceof Error?e.message:'Could not sign in')}
    finally{setBusy(false)}
  }
  return <div className="fixed inset-0 z-50 flex items-center justify-center bg-slate-950/60 p-4 backdrop-blur-sm" onMouseDown={e=>{if(e.target===e.currentTarget)onClose()}}>
    <div className="panel relative w-full max-w-md p-7 sm:p-9">
      <button onClick={onClose} className="absolute right-5 top-5 rounded-lg p-2 hover:bg-slate-100" aria-label={t('Close')}><X size={18}/></button>
      <div className="mb-5 flex h-12 w-12 items-center justify-center rounded-2xl bg-blue-50 text-blue-600"><LockKeyhole size={24}/></div>
      <h2 className="text-2xl font-bold text-slate-900">{t(register?'Create an account':'Welcome back')}</h2>
      <p className="mt-1 text-sm text-slate-500">{t(register?'Save your own CTU credentials securely for reservations.':'Sign in to request a library room.')}</p>
      <div className="mt-6 flex rounded-xl bg-slate-100 p-1 text-sm font-semibold">
        <button className={`flex-1 rounded-lg py-2 ${!register?'bg-white shadow-sm text-blue-700':'text-slate-500'}`} onClick={()=>{setRegister(false);setError('')}}>{t('Sign in')}</button>
        <button className={`flex-1 rounded-lg py-2 ${register?'bg-white shadow-sm text-blue-700':'text-slate-500'}`} onClick={()=>{setRegister(true);setError('')}}>{t('Register')}</button>
      </div>
      <form className="mt-5 space-y-4" onSubmit={submit}>
        <label className="block"><span className="field-label">{t('App username')}</span><input className="field" autoComplete="username" required minLength={3} maxLength={32} value={username} onChange={e=>setUsername(e.target.value)} placeholder="your_username" /></label>
        <label className="block"><span className="field-label">{t('App password')}</span><input className="field" autoComplete={register?'new-password':'current-password'} required type="password" minLength={register?10:1} value={password} onChange={e=>setPassword(e.target.value)} placeholder="••••••••••••" /></label>
        {register&&<>
          <div className="border-t border-slate-100 pt-4 text-sm text-slate-500">{t('Your CTU credentials are stored separately (encrypted), and are only sent to CTU when you make a booking.')}</div>
          <label className="block"><span className="field-label">{t('CTU username / student ID')}</span><input className="field" autoComplete="off" required value={ctuUsername} onChange={e=>setCtuUsername(e.target.value)} placeholder="B26…" /></label>
          <label className="block"><span className="field-label">{t('CTU password')}</span><input className="field" autoComplete="new-password" required type="password" value={ctuPassword} onChange={e=>setCtuPassword(e.target.value)} placeholder={t('CTU account password')} /></label>
        </>}
        {error&&<Notice kind="error">{t(error)}</Notice>}
        <button className="btn-primary w-full" disabled={busy}>{t(busy?'Please wait…':register?'Create account':'Sign in')} <ArrowRight size={16}/></button>
      </form>
    </div>
  </div>
}

function BookingModal({slot,room,onClose,onDone,onScheduled}:{slot:Slot,room:Room,onClose:()=>void,onDone:(b:Booking)=>void,onScheduled:()=>void}){
  const {t,language}=useI18n()
  const [students,setStudents]=useState(5)
  const [mode,setMode]=useState<'now'|'later'>(slot.status==='available'||slot.status==='vacant'?'now':'later')
  const [runAt,setRunAt]=useState('')
  const [note,setNote]=useState('')
  const [borrowTv,setBorrowTv]=useState(false)
  const [busy,setBusy]=useState(false)
  const [error,setError]=useState('')
  async function submit(e:FormEvent){
    e.preventDefault();setBusy(true);setError('')
    try {
      const payload={room_id:room.id,date:slot.date,slot:slot.slot,students,note,borrow_tv:borrowTv}
      if(mode==='later'){
        if(!runAt||Number.isNaN(new Date(runAt).getTime()))throw new Error('Choose a valid date and time for the attempt')
        await api('/api/jobs','POST',{...payload,run_at:new Date(runAt).toISOString()})
        onScheduled()
      }else {const b=await api<Booking>('/api/bookings','POST',payload);onDone(b)}
    }
    catch(e){setError(e instanceof Error?e.message:'Could not reserve the room')}
    finally{setBusy(false)}
  }
  return <div className="fixed inset-0 z-50 flex items-center justify-center bg-slate-950/60 p-4 backdrop-blur-sm" onMouseDown={e=>{if(e.target===e.currentTarget&&!busy)onClose()}}>
    <div className="panel relative w-full max-w-lg p-7 sm:p-9">
      <button onClick={onClose} className="absolute right-5 top-5 rounded-lg p-2 hover:bg-slate-100" aria-label={t('Close')}><X size={18}/></button>
      <p className="mb-2 text-xs font-bold uppercase tracking-widest text-blue-600">{t('Confirm request')}</p>
      <h2 className="text-2xl font-bold">{t(mode==='later'?'Schedule an attempt':'Reserve this room')}</h2>
      <div className="my-6 rounded-2xl bg-slate-50 p-5 space-y-2 text-sm">
        <div className="flex items-start gap-3 font-semibold"><BookOpen className="text-blue-600" size={18}/>{room.name}</div>
        <div className="flex items-center gap-3 text-slate-600"><CalendarDays size={18}/>{fullDate(slot.date,language)}</div>
        <div className="flex items-center gap-3 text-slate-600"><Clock3 size={18}/>{t('Shift')} {slot.slot}{slotTimes[slot.slot] ? ` · ${slotTimes[slot.slot]}` : ''}</div>
      </div>
      <form onSubmit={submit} className="space-y-4">
        {(slot.status==='available'||slot.status==='vacant')&&<div className="flex rounded-xl bg-slate-100 p-1 text-sm font-semibold"><button type="button" onClick={()=>setMode('now')} className={`flex-1 rounded-lg py-2 ${mode==='now'?'bg-white text-blue-700 shadow-sm':'text-slate-500'}`}>{t('Book now')}</button><button type="button" onClick={()=>setMode('later')} className={`flex-1 rounded-lg py-2 ${mode==='later'?'bg-white text-blue-700 shadow-sm':'text-slate-500'}`}>{t('Schedule later')}</button></div>}
        {mode==='later'&&<label className="block"><span className="field-label">{t('When should the app attempt this booking? (your local time)')}</span><input className="field" type="datetime-local" required value={runAt} onChange={e=>setRunAt(e.target.value)}/></label>}
        <label className="block"><span className="field-label">{t('Group size (5–20 students)')}</span><input className="field" type="number" min={5} max={20} required value={students} onChange={e=>setStudents(Number(e.target.value))}/></label>
        <label className="block"><span className="field-label">{t('Purpose / note (optional)')}</span><textarea className="field resize-none" rows={3} maxLength={200} value={note} onChange={e=>setNote(e.target.value)} placeholder={t('Group study or discussion')}/></label>
        <label className="flex items-center gap-3 text-sm text-slate-700"><input type="checkbox" checked={borrowTv} onChange={e=>setBorrowTv(e.target.checked)} className="h-4 w-4 accent-blue-600"/>{t('Request a TV')}</label>
        <Notice>{t(mode==='later'?'This schedules one attempt, not a reservation. An empty slot may not yet be open for booking; CTU decides whether it accepts the request.':'CTU makes the final decision. Empty does not necessarily mean bookable; the app rechecks after logging into CTU. Only an explicit CTU confirmation counts as success.')}</Notice>
        {error&&<Notice kind="error">{t(error)}</Notice>}
        <button disabled={busy} className="btn-primary w-full">{t(busy?'Processing…':mode==='later'?'Create scheduled attempt':'Submit reservation')}<ArrowRight size={16}/></button>
      </form>
    </div>
  </div>
}


function PreBookModal({room,onClose,onScheduled}:{room:Room,onClose:()=>void,onScheduled:()=>void}){
  const {t,language}=useI18n()
  const shifts=room.id<=3?[1,2,3,4,5,6]:room.id===4?[7,8,9]:[10,11,12,13]
  const [date,setDate]=useState(vnDatePlusDays(14))
  const [slot,setSlot]=useState(shifts[0])
  const [openingTime,setOpeningTime]=useState('')
  const [students,setStudents]=useState(5)
  const [note,setNote]=useState('')
  const [borrowTv,setBorrowTv]=useState(false)
  const [busy,setBusy]=useState(false)
  const [error,setError]=useState('')
  const openingDate=date?minusDays(date,13):''
  async function submit(e:FormEvent){
    e.preventDefault();setError('');setBusy(true)
    try{
      if(!openingTime)throw new Error('Choose the actual CTU booking-opening hour in Vietnam time')
      await api('/api/jobs','POST',{
        kind:'prebook',room_id:room.id,date,slot,students,note,borrow_tv:borrowTv,open_time_vn:openingTime
      })
      onScheduled()
    }catch(e){setError(e instanceof Error?e.message:'Unable to schedule pre-booking')}
    finally{setBusy(false)}
  }
  return <div className="fixed inset-0 z-50 flex items-center justify-center bg-slate-950/60 p-4 backdrop-blur-sm" onMouseDown={e=>{if(e.target===e.currentTarget&&!busy)onClose()}}>
    <div className="panel relative w-full max-w-lg max-h-[90vh] overflow-y-auto p-7 sm:p-9">
      <button onClick={onClose} className="absolute right-5 top-5 rounded-lg p-2 hover:bg-slate-100" aria-label={t('Close')}><X size={18}/></button>
      <div className="mb-4 flex h-12 w-12 items-center justify-center rounded-2xl bg-blue-50 text-blue-600"><CalendarDays size={24}/></div>
      <h2 className="text-2xl font-bold text-slate-900">{t('Pre-book 13 days ahead')}</h2>
      <p className="mt-2 text-sm leading-6 text-slate-500">{t('Choose the date you want the room (Day A). The server schedules the attempt for Day A minus 13 days, using Vietnam time (UTC+07).')}</p>
      <div className="mt-5 rounded-2xl bg-slate-50 p-4 text-sm font-semibold text-slate-800"><BookOpen className="mr-2 inline text-blue-600" size={17}/>{room.name}</div>
      <form className="mt-5 space-y-4" onSubmit={submit}>
        <label className="block"><span className="field-label">{t('Day A — target reservation date')}</span><input className="field" required type="date" min={vnDatePlusDays(13)} max={vnDatePlusDays(90)} value={date} onChange={e=>setDate(e.target.value)}/></label>
        <label className="block"><span className="field-label">{t('Shift')}</span><select className="field" value={slot} onChange={e=>setSlot(Number(e.target.value))}>{shifts.map(s=><option value={s} key={s}>{t('Shift')} {s}{slotTimes[s]?` · ${slotTimes[s]}`:''}</option>)}</select></label>
        <label className="block"><span className="field-label">{t('Official CTU booking-opening hour (HH:MM, Vietnam)')}</span><input className="field" type="time" required value={openingTime} onChange={e=>setOpeningTime(e.target.value)}/><span className="mt-1 block text-xs text-slate-500">{t('The attached timetable does not specify this hour. Verify it with the library; the app does not assume midnight.')}</span></label>
        <div className="rounded-2xl border border-blue-200 bg-blue-50 px-4 py-4 text-sm"><div className="font-bold text-blue-800">{t('Automatic attempt (Vietnam time)')}</div><div className="mt-1 text-lg font-extrabold text-blue-900">{openingDate||'—'} {language==='vi'?'lúc':'at'} {openingTime||'HH:MM'} ICT</div><div className="mt-1 text-xs text-blue-700">{t('13 calendar days before')} {date||t('the target date')}</div></div>
        <label className="block"><span className="field-label">{t('Group size (5–20)')}</span><input className="field" type="number" min={5} max={20} required value={students} onChange={e=>setStudents(Number(e.target.value))}/></label>
        <label className="block"><span className="field-label">{t('Purpose / note (optional)')}</span><textarea className="field resize-none" rows={2} maxLength={200} value={note} onChange={e=>setNote(e.target.value)}/></label>
        <label className="flex items-center gap-3 text-sm text-slate-700"><input type="checkbox" checked={borrowTv} onChange={e=>setBorrowTv(e.target.checked)} className="h-4 w-4 accent-blue-600"/>{t('Request a TV')}</label>
        <Notice>{t('The job is saved in SQLite and survives restarts. At opening time, the server checks CTU and attempts booking only for an empty or bookable slot. It briefly rechecks unopened slots; it never repeats an uncertain CTU submission. This does not guarantee a reservation.')}</Notice>
        {error&&<Notice kind="error">{t(error)}</Notice>}
        <button className="btn-primary w-full" disabled={busy}>{t(busy?'Saving…':'Schedule pre-booking')}<ArrowRight size={16}/></button>
      </form>
    </div>
  </div>
}

function App(){
  const {t,language,setLanguage}=useI18n()
  const [schedule,setSchedule]=useState<Schedule|null>(null)
  const [user,setUser]=useState<User|null>(null)
  const [bookings,setBookings]=useState<Booking[]>([])
  const [jobs,setJobs]=useState<Job[]>([])
  const [selectedRoom,setSelectedRoom]=useState(1)
  const [tab,setTab]=useState<'schedule'|'bookings'|'jobs'|'account'>('schedule')
  const [authOpen,setAuthOpen]=useState(false)
  const [bookingSlot,setBookingSlot]=useState<Slot|null>(null)
  const [preBookOpen,setPreBookOpen]=useState(false)
  const [bookingResult,setBookingResult]=useState<Booking|null>(null)
  const [loading,setLoading]=useState(true)
  const [refreshing,setRefreshing]=useState(false)
  const [error,setError]=useState('')
  const [link,setLink]=useState('')
  const [linkBusy,setLinkBusy]=useState(false)
  const refreshSchedule=useCallback(async (force=false)=>{
    setRefreshing(true)
    try {setSchedule(await api<Schedule>(force?'/api/schedule/refresh':'/api/schedule',force?'POST':'GET'));setError('')}
    catch(e){setError(e instanceof Error?e.message:'Could not load schedule')}
    finally{setRefreshing(false)}
  },[])
  const refreshJobs=useCallback(async()=>{try{setJobs(await api<Job[]>('/api/jobs'))}catch{setJobs([])}},[])
  const refreshBookings=useCallback(async()=>{
    try {setBookings(await api<Booking[]>('/api/bookings'))}catch{setBookings([])}
  },[])
  useEffect(()=>{
    Promise.all([api<Schedule>('/api/schedule'),api<User>('/api/auth/me').catch(()=>null)]).then(([s,u])=>{
      setSchedule(s);setUser(u);if(u){void refreshBookings();void refreshJobs()}
    }).catch(e=>setError(e instanceof Error?e.message:'Could not load')).finally(()=>setLoading(false))
  },[refreshBookings,refreshJobs])
  useEffect(()=>{const timer=setInterval(()=>{void refreshSchedule()},60000);return()=>clearInterval(timer)},[refreshSchedule])
  const room=schedule?.rooms.find(r=>r.id===selectedRoom)??schedule?.rooms[0]
  const dates=useMemo(()=>Array.from(new Set(schedule?.slots.map(s=>s.date)??[])).sort(),[schedule])
  const shifts=room?(room.id<=3?[1,2,3,4,5,6]:room.id===4?[7,8,9]:[10,11,12,13]):[]
  const slots=useMemo(()=>new Map((schedule?.slots??[]).filter(s=>s.room_id===room?.id).map(s=>[`${s.date}/${s.slot}`,s] as const)),[schedule,room])
  const available=Array.from(slots.values()).filter(s=>s.status==='available'||s.status==='vacant').length
  const reserved=Array.from(slots.values()).filter(s=>s.status==='reserved').length
  async function logout(){try{await api<unknown>('/api/auth/logout','POST');setUser(null);setBookings([]);setJobs([]);setTab('schedule');setLink('')}catch(e){setError(e instanceof Error?e.message:'Logout failed')}}
  async function getTelegramLink(){setLinkBusy(true);setError('');try{const d=await api<{url:string}>('/api/telegram/link','POST');setLink(d.url)}catch(e){setError(e instanceof Error?e.message:'Telegram link unavailable')}finally{setLinkBusy(false)}}
  async function cancelJob(id:number){try{await api(`/api/jobs/${id}`,'DELETE');await refreshJobs()}catch(e){setError(e instanceof Error?e.message:'Could not cancel scheduled attempt')}}
  async function unlinkTelegram(){try{await api('/api/telegram/unlink','POST');setUser(u=>u?{...u,telegram_linked:false,telegram_username:null}:u);setLink('')}catch(e){setError(e instanceof Error?e.message:'Could not unlink')}}
  async function updateMe(){try{setUser(await api<User>('/api/auth/me'))}catch{}}
  return <div className="min-h-screen bg-[#f7f9fc]">
    <div className="pointer-events-none absolute inset-x-0 top-0 h-80 bg-gradient-to-br from-blue-50 via-white to-cyan-50"/>
    <div className="relative mx-auto max-w-[1440px] px-4 pb-16 sm:px-7 lg:px-10">
      <header className="flex flex-wrap items-center justify-between gap-4 border-b border-slate-200/70 py-5">
        <div className="flex items-center gap-3"><div className="flex h-11 w-11 items-center justify-center rounded-2xl bg-gradient-to-br from-blue-600 to-cyan-500 text-white shadow-lg shadow-blue-200"><BookOpen size={23}/></div>
          <div><div className="text-lg font-extrabold tracking-tight text-slate-900">ITClub-<span className="text-blue-600">AutoRoom</span></div><div className="text-[11px] font-medium uppercase tracking-widest text-slate-400">{t('Library reservation portal')}</div></div></div>
        <div className="flex items-center gap-2 sm:gap-3"><div className="inline-flex rounded-lg border border-slate-200 bg-white p-1 text-xs font-semibold" role="group" aria-label="Language / Ngôn ngữ"><button type="button" lang="vi" aria-pressed={language==='vi'} onClick={()=>setLanguage('vi')} className={`rounded-md px-2 py-1.5 ${language==='vi'?'bg-blue-600 text-white':'text-slate-600'}`}>VI</button><button type="button" lang="en" aria-pressed={language==='en'} onClick={()=>setLanguage('en')} className={`rounded-md px-2 py-1.5 ${language==='en'?'bg-blue-600 text-white':'text-slate-600'}`}>EN</button></div><a className="btn-ghost hidden sm:inline-flex" href="https://libapp.ctu.edu.vn/dkptl/" target="_blank" rel="noreferrer">{t('Official CTU site')} <ExternalLink size={14}/></a>
          {user?<><span className="hidden rounded-full bg-white px-4 py-2 text-sm font-semibold text-slate-700 shadow-sm sm:block">{user.username}</span><button title={t('Log out')} className="btn-ghost" onClick={()=>void logout()}><LogOut size={18}/><span className="hidden sm:inline">{t('Logout')}</span></button></>:<button className="btn-primary" onClick={()=>setAuthOpen(true)}>{t('Sign in')} <ArrowRight size={16}/></button>}
        </div>
      </header>
      <main className="pt-9">
        <div className="flex flex-col gap-6 lg:flex-row lg:items-end lg:justify-between">
          <div><div className="mb-3 inline-flex items-center gap-2 rounded-full bg-blue-100 px-3 py-1 text-xs font-semibold text-blue-700"><span className="h-1.5 w-1.5 rounded-full bg-blue-600"/> {t('Powered by CTU live calendar')}</div>
            <h1 className="text-3xl font-extrabold tracking-tight text-slate-900 sm:text-4xl">{t('Find your space to')} <span className="text-blue-600">{t('focus.')}</span></h1>
            <p className="mt-2 max-w-2xl text-sm leading-7 text-slate-500 sm:text-base">{t('Explore discussion rooms, see their availability, and request a study session with your CTU account.')}</p>
          </div>
          <div className="flex flex-wrap items-center gap-3"><span className="text-xs leading-5 text-slate-500">{t('Last synced')}<br/><strong className="font-semibold text-slate-700">{timeAgo(schedule?.synced_at??null,language)}</strong></span>
            <button className="btn-primary" onClick={()=>user?setPreBookOpen(true):setAuthOpen(true)}><CalendarDays size={16}/> {t('Pre-book 13 days ahead')}</button>
            <button className="btn-ghost border border-slate-200 bg-white shadow-sm" onClick={()=>void refreshSchedule(Boolean(user))} disabled={refreshing}>{<RefreshCw size={16} className={refreshing?'animate-spin':''}/>} {t('Refresh')}</button></div>
        </div>
        <div className="mt-7 flex items-center gap-2 overflow-x-auto border-b border-slate-200">
          {([['schedule','Availability',LayoutGrid],['bookings','My bookings',CalendarDays],['jobs','Scheduled attempts',Clock3],['account','Telegram & account',Bell]] as const).map(([key,label,Icon])=><button key={key} onClick={()=>{if(!user&&key!=='schedule'){setAuthOpen(true);return}setTab(key);if(key==='bookings')void refreshBookings();if(key==='jobs')void refreshJobs()}} className={`inline-flex shrink-0 items-center gap-2 border-b-2 px-4 pb-3 pt-2 text-sm font-semibold ${tab===key?'border-blue-600 text-blue-700':'border-transparent text-slate-500 hover:text-slate-800'}`}><Icon size={17}/>{t(label)}</button>)}
        </div>
        {error&&<div className="mt-5"><Notice kind="error">{t(error)}</Notice></div>}
        {schedule?.stale&&<div className="mt-5"><Notice>{t('Schedule data is stale or from an offline snapshot. You can inspect it, but every reservation still requires a fresh CTU check. Empty but non-clickable CTU cells are shown separately as tentative vacancies; CTU confirms eligibility when booking.')}</Notice></div>}
        {loading&&<div className="panel mt-7 p-16 text-center text-slate-500">{t('Loading schedule…')}</div>}
        {!loading&&tab==='schedule'&&<>
          <div className="mt-6 grid gap-4 sm:grid-cols-3">
            <div className="panel p-5"><span className="flex items-center gap-2 text-sm text-slate-500"><BookOpen size={16} className="text-blue-500"/> {t('Rooms monitored')}</span><div className="mt-2 text-3xl font-extrabold">{schedule?.rooms.length??0}</div></div>
            <div className="panel p-5"><span className="flex items-center gap-2 text-sm text-slate-500"><CheckCircle2 size={16} className="text-emerald-500"/> {t('Empty shifts in selected room')}</span><div className="mt-2 text-3xl font-extrabold text-emerald-600">{available}</div></div>
            <div className="panel p-5"><span className="flex items-center gap-2 text-sm text-slate-500"><Users size={16} className="text-rose-500"/> {t('Reserved shifts in selected room')}</span><div className="mt-2 text-3xl font-extrabold">{reserved}</div></div>
          </div>
          <div className="mt-7 grid gap-6 lg:grid-cols-[270px_minmax(0,1fr)]">
            <aside className="panel h-fit p-4"><div className="mb-3 px-2 pt-2 text-xs font-bold uppercase tracking-widest text-slate-400">{t('Choose a room')}</div><div className="space-y-2">
              {[
                {title:'Discussion Rooms A–C',rooms:(schedule?.rooms??[]).filter(r=>r.id>=1&&r.id<=3)},
                {title:'Other library rooms',rooms:(schedule?.rooms??[]).filter(r=>r.id>3)}
              ].map(group=>group.rooms.length>0&&<div key={group.title} className="space-y-2">
                <div className="px-2 pt-2 text-[11px] font-bold uppercase tracking-widest text-slate-400">{t(group.title)}</div>
                {group.rooms.map(r=><button key={r.id} onClick={()=>setSelectedRoom(r.id)} className={`w-full rounded-xl border px-3 py-3.5 text-left ${room?.id===r.id?'border-blue-200 bg-blue-50 text-blue-900':'border-transparent text-slate-600 hover:bg-slate-50'}`}><div className="text-sm font-bold leading-5">{r.name}</div><div className="mt-1 flex items-center gap-1 text-xs opacity-75"><MapPin size={12}/> {t('Floor')} {r.floor}</div></button>)}
              </div>)}
            </div></aside>
            <section className="panel min-w-0 overflow-hidden">
              <div className="flex flex-wrap items-center justify-between gap-3 border-b border-slate-100 p-5 sm:p-6"><div><h2 className="text-lg font-bold text-slate-900">{room?.name??t('Room availability')}</h2><p className="mt-1 text-sm text-slate-500">{t('Choose an empty or bookable shift; CTU confirms eligibility at submission.')}</p></div><span className="rounded-full bg-blue-50 px-3 py-1.5 text-xs font-semibold text-blue-700">{t('Floor')} {room?.floor??'-'}</span></div>
              <div className="flex flex-wrap gap-4 px-5 py-4 text-xs text-slate-500 sm:px-6">{(Object.keys(states) as Status[]).map(k=><span key={k} className="inline-flex items-center gap-1.5"><span className={`h-2.5 w-2.5 rounded-full ${states[k].dot}`}/>{t(states[k].label)}</span>)}</div>
              <div className="overflow-x-auto px-4 pb-6 sm:px-6"><table className="w-full min-w-[610px] border-separate border-spacing-y-2 text-sm"><thead><tr><th className="sticky left-0 z-10 w-32 bg-white px-3 py-2 text-left text-xs font-semibold uppercase tracking-widest text-slate-400">{t('Date')}</th>{shifts.map(s=><th key={s} className="min-w-24 px-1 py-2 text-center text-xs font-bold text-slate-500">{t('Shift')} {s}{slotTimes[s]&&<span className="mt-1 block text-[10px] font-normal">{slotTimes[s]}</span>}</th>)}</tr></thead><tbody>
                {dates.map(d=><tr key={d}><th className="sticky left-0 z-10 bg-white px-3 py-2 text-left"><div className="font-bold text-slate-800">{dateLabel(d,language)}</div><div className="mt-0.5 text-[11px] font-normal text-slate-400">{d}</div></th>{shifts.map(s=>{const cell=slots.get(`${d}/${s}`);const status=cell?.status??'unavailable';return <td key={s} className="px-1 py-1"><button title={`${fullDate(d,language)}, ${t('Shift')} ${s}: ${t(states[status].label)}`} className={`w-full min-h-11 rounded-xl border px-1.5 py-2 text-xs font-semibold ${states[status].cls}`} disabled={status==='reserved'||status==='closed'} onClick={()=>cell&&(user?setBookingSlot(cell):setAuthOpen(true))}>{t(status==='unavailable'?'Plan attempt':status==='vacant'?'Empty · verify':states[status].label)}</button></td>})}</tr>)}
              </tbody></table>{!dates.length&&<p className="py-10 text-center text-slate-500">{t('No calendar loaded yet. Verify CTU connectivity and refresh.')}</p>}</div>
            </section>
          </div>
        </>}
        {!loading&&tab==='bookings'&&<section className="panel mt-6 overflow-hidden"><div className="flex items-center justify-between border-b border-slate-100 p-6"><div><h2 className="text-xl font-bold">{t('My reservations')}</h2><p className="mt-1 text-sm text-slate-500">{t('History of CTU submissions under your account')}</p></div><button onClick={()=>void refreshBookings()} className="btn-ghost"><RefreshCw size={16}/></button></div>
          {bookings.length===0?<div className="px-6 py-20 text-center text-slate-500"><CalendarDays className="mx-auto mb-4 text-slate-300" size={36}/>{t('No reservations yet.')}</div>:<div className="divide-y divide-slate-100">{bookings.map(b=><div key={b.id} className="flex flex-col justify-between gap-3 p-5 sm:flex-row sm:items-center sm:px-6"><div><div className="font-bold">{b.room_name}</div><p className="mt-1 text-sm text-slate-500">{dateLabel(b.date,language)} · {t('Shift')} {b.slot} · {b.students} {t('students')}</p><p className="mt-1 text-xs text-slate-500">{t(b.message)}</p></div><span className={`self-start rounded-full px-3 py-1.5 text-xs font-bold ${b.status==='confirmed'?'bg-green-100 text-green-700':b.status==='failed'?'bg-red-100 text-red-700':b.status==='uncertain'?'bg-amber-100 text-amber-800':'bg-slate-100 text-slate-700'}`}>{t(b.status)}</span></div>)}</div>}
        </section>}
        {!loading&&tab==='jobs'&&<section className="panel mt-6 overflow-hidden"><div className="flex items-center justify-between border-b border-slate-100 p-6"><div><h2 className="text-xl font-bold">{t('Scheduled attempts')}</h2><p className="mt-1 text-sm text-slate-500">{t('Pre-booking starts 13 days before your target date. A scheduled job is not a reservation. SQLite keeps pending tasks after a crash. The event-driven scheduler wakes at the next deadline and on new jobs, with a 30-second recovery check. Possible CTU submissions are never repeated automatically.')}</p></div><button onClick={()=>void refreshJobs()} className="btn-ghost"><RefreshCw size={16}/></button></div>
          {jobs.length===0?<div className="px-6 py-20 text-center text-slate-500"><Clock3 className="mx-auto mb-4 text-slate-300" size={36}/>{t('No scheduled attempts yet. Use Pre-book 13 days ahead or choose a shift in Availability.')}</div>:<div className="divide-y divide-slate-100">{jobs.map(j=><div key={j.id} className="flex flex-col justify-between gap-3 p-5 sm:flex-row sm:items-center sm:px-6"><div><div className="font-bold">{j.room_name} {j.kind==='prebook'&&<span className="ml-2 rounded-md bg-blue-50 px-2 py-1 text-[10px] font-bold uppercase text-blue-700">{t('13-day pre-book')}</span>}</div><p className="mt-1 text-sm text-slate-500">{dateLabel(j.date,language)} · {t('Shift')} {j.slot} · {j.students} {t('students')}</p><p className="mt-1 text-xs text-slate-500">{t(j.kind==='prebook'?'Opens':'Attempt')}: {timeAgo(j.opens_at??j.run_at,language)} · {t('Attempts')}: {j.attempts} · {t('Restarts recovered')}: {j.recovery_count} · {t(j.message||'Awaiting scheduled time')}</p></div><div className="flex items-center gap-2"><span className={`rounded-full px-3 py-1.5 text-xs font-bold ${j.status==='confirmed'?'bg-emerald-100 text-emerald-700':j.status==='scheduled'?'bg-blue-100 text-blue-700':j.status==='uncertain'?'bg-amber-100 text-amber-700':'bg-slate-100 text-slate-600'}`}>{t(j.status)}</span>{j.status==='scheduled'&&<button onClick={()=>void cancelJob(j.id)} className="btn-ghost text-red-600">{t('Cancel')}</button>}</div></div>)}</div>}
        </section>}
        {!loading&&tab==='account'&&user&&<div className="mt-6 grid gap-6 lg:grid-cols-2"><div className="panel p-7"><h2 className="text-xl font-bold">{t('Account details')}</h2><div className="mt-6 space-y-5 text-sm"><div><div className="text-slate-400">{t('App username')}</div><div className="mt-1 font-semibold">{user.username}</div></div><div><div className="text-slate-400">{t('Linked CTU account')}</div><div className="mt-1 font-semibold">{user.ctu_username}</div></div><div className="flex gap-2 rounded-xl bg-blue-50 p-4 text-blue-800"><ShieldCheck size={18} className="shrink-0"/><span>{t('CTU passwords are encrypted at rest and never sent to the browser after registration.')}</span></div></div></div>
          <div className="panel p-7"><div className="flex items-center gap-3"><Bell className="text-blue-600"/><h2 className="text-xl font-bold">{t('Telegram notifications')}</h2></div><p className="mt-3 text-sm leading-6 text-slate-500">{t('Connect your Telegram chat once to receive confirmed, failed, or uncertain booking outcomes. Telegram requires starting the bot first; a phone number or @username alone cannot start a private chat.')}</p>
            <div className="mt-5">{user.telegram_linked?<><Notice kind="success">{t('Telegram linked')} {user.telegram_username?`(@${user.telegram_username})`:''}.</Notice><button className="btn-ghost mt-3 border border-slate-200" onClick={()=>void unlinkTelegram()}>{t('Disconnect Telegram')}</button></>:<Notice>{t('Telegram is not linked yet.')}</Notice>}</div>
            <div className="mt-4 flex flex-wrap gap-2"><button className="btn-primary" onClick={()=>void getTelegramLink()} disabled={linkBusy}><Link2 size={16}/>{t(linkBusy?'Generating…':'Get one-time bot link')}</button><button className="btn-ghost border border-slate-200" onClick={()=>void updateMe()}><RefreshCw size={16}/> {t('Check connection')}</button></div>
            {link&&<div className="mt-4 break-all rounded-xl bg-slate-50 p-3 text-sm"><a href={link} target="_blank" rel="noopener noreferrer" className="font-semibold text-blue-700 underline">{t('Open Telegram bot')} <ExternalLink size={14} className="inline"/></a><p className="mt-2 text-xs text-slate-500">{t('Expires in 10 minutes. Do not share this link.')}</p></div>}
          </div></div>}
        <footer className="mt-12 border-t border-slate-200 py-6 text-center text-xs leading-6 text-slate-400">{t('Unofficial student-operated interface')} · {t('CTU remains the source of truth')} · <a className="text-blue-600 underline" href="https://libapp.ctu.edu.vn/dkptl/" target="_blank" rel="noreferrer">{t('Official reservation site')}</a></footer>
      </main>
    </div>
    {authOpen&&<AuthForm onDone={u=>{setUser(u);setAuthOpen(false);void refreshBookings();void refreshJobs()}} onClose={()=>setAuthOpen(false)}/>}
    {preBookOpen&&room&&<PreBookModal room={room} onClose={()=>setPreBookOpen(false)} onScheduled={()=>{setPreBookOpen(false);setTab('jobs');void refreshJobs()}}/>}
    {bookingSlot&&room&&<BookingModal slot={bookingSlot} room={room} onClose={()=>setBookingSlot(null)} onDone={b=>{setBookingSlot(null);setBookingResult(b);void refreshBookings();void refreshSchedule()}} onScheduled={()=>{setBookingSlot(null);setTab('jobs');void refreshJobs()}}/>}
    {bookingResult&&<div className="fixed inset-0 z-50 flex items-center justify-center bg-slate-950/60 p-4 backdrop-blur-sm"><div className="panel max-w-md p-8 text-center"><div className="mx-auto mb-4 flex h-14 w-14 items-center justify-center rounded-full bg-blue-50 text-blue-700">{bookingResult.status==='confirmed'?<CheckCircle2 size={27}/>:<Bell size={27}/>}</div><h2 className="text-xl font-bold">{t(bookingResult.status==='confirmed'?'Reservation confirmed':bookingResult.status==='uncertain'?'Confirmation could not be verified':'Reservation not confirmed')}</h2><p className="mt-3 text-sm leading-6 text-slate-500">{t(bookingResult.message)}</p><p className="mt-3 text-xs text-slate-400">{bookingResult.room_name} · {bookingResult.date} · {t('Shift')} {bookingResult.slot}</p><button className="btn-primary mt-6 w-full" onClick={()=>{setBookingResult(null);setTab('bookings')}}>{t('View my reservations')} <ArrowRight size={16}/></button></div></div>}
  </div>
}
export default App

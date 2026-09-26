const {test} = require('node:test');
const assert = require('node:assert/strict');
const vm = require('node:vm');
const fs = require('node:fs');
const source = fs.readFileSync('web/app.js','utf8');

test('report markdown renders readable headings and escapes artifact HTML',()=>{
  const s={}; vm.createContext(s);
  vm.runInContext(source.slice(source.indexOf('const escapeHTML ='),source.indexOf('const names ='))+source.slice(source.indexOf('function renderMarkdown('),source.indexOf('function filteredJobs(')),s);
  const html=s.renderMarkdown('# Summary\n\n- finding\n\n| Item | Result |\n| --- | --- |\n| A | Pass |\n\n```\n<script>alert(1)</script>\n```');
  assert.match(html,/<h1>Summary<\/h1>/);
  assert.match(html,/<li>finding<\/li>/);
  assert.match(html,/<td>Pass<\/td>/);
  assert.doesNotMatch(html,/<script>/);
});

test('attention dismissal hides only acknowledged terminal jobs and offers authorized restoration',()=>{
  const s={access:{operable_repositories:['repo']},escapeHTML:String};
  vm.createContext(s);
  vm.runInContext(source.slice(source.indexOf('function needsAttention('),source.indexOf('const attentionPending =')),s);
  const job={id:'job',repository:'repo',status:'rejected'};
  assert.equal(s.needsAttention(job),true);
  assert.match(s.attentionAction(job),/>Dismiss</);
  job.attention_dismissed=true;
  assert.equal(s.needsAttention(job),false);
  assert.match(s.attentionAction(job),/Restore to Needs attention/);
  s.access.operable_repositories=[];
  assert.equal(s.attentionAction(job),'');
  job.status='running';job.attention_dismissed=false;
  assert.equal(s.needsAttention(job),false);
});
test('legacy timeout receipts distinguish unclaimed and running workers',()=>{
  const s = {};
  vm.createContext(s);
  vm.runInContext(source.slice(source.indexOf('function attemptStatus('),source.indexOf('function renderDetail(')),s);
  const a={status:'timed_out',started_at:null,error:'Attempt deadline elapsed'};
  assert.equal(s.attemptStatus(a),'launch_timed_out');
  assert.match(s.attemptError(a),/^Worker launch timed out:/);
  a.started_at='2026-09-18T22:00:00Z';
  assert.equal(s.attemptStatus(a),'execution_timed_out');
  assert.match(s.attemptError(a),/^Phase execution timed out:/);
  a.status='failed';a.error='Worker heartbeat expired';
  assert.equal(s.attemptStatus(a),'failed');
  assert.equal(s.attemptError(a),'Worker heartbeat expired');
});
function setup() {
  const elements = new Map();
  const element = id => {
    if (!elements.has(id)) elements.set(id, {checked:false, hidden:true, disabled:false, textContent:'', showModal(){this.open=true}, close(){this.open=false},addEventListener(){}});
    return elements.get(id);
  };
  const requests=[];
  const sandbox={state:{catalog:{repositories:[{id:"repo"}]}},Date,Map,crypto:{randomUUID:()=> 'decision-event'},sessionStorage:{getItem:()=> 'test-token'},$ : element,escapeHTML:String,short:s=>s.slice(0,8),toast(){},refresh:async()=>{},fetch:async(url,options)=>{requests.push({url,...options});return {ok:true,json:async()=>({})}}};
  vm.createContext(sandbox);
  vm.runInContext(`let access={subject:'maintainer',approvable_repositories:['repo']}, accessVersion=0, pendingDecision=null, decisionBusy=false; const decisionIds=new Map(); let detailData=null;`+source.slice(source.indexOf('function canApproveRepository('),source.indexOf('function artifactRow('))+`;globalThis.configure=(job,allowed=true)=>{detailData={job};access.approvable_repositories=allowed?['repo']:[]};globalThis.changeDigest=()=>detailData.job.gates[0].artifact_digest='new-digest';`,sandbox);
  const gate={id:'gate',phase:'research',attempt_id:'attempt',status:'pending',artifact_digest:'reviewed-digest',deadline:new Date(Date.now()+60000).toISOString()};
  const job={id:'job',issue:{key:'TEST'},repository:{id:'repo'},workflow:'demo',snapshot:{workflows:{demo:{phases:[{id:'research',gate:{channels:['api']}}]}}},gates:[gate]};
  sandbox.configure(job);
  return {sandbox,job,gate,element,requests};
}
test('buttons require repository authority, pending status and API channel',()=>{
  const {sandbox:s,job,gate}=setup();
  assert.match(s.gateActions(job,gate),/>Approve</);
  s.configure(job,false); assert.doesNotMatch(s.gateActions(job,gate),/data-decide/);
  s.configure(job); gate.status='approved'; assert.equal(s.gateActions(job,gate),'');
  gate.status='pending';job.snapshot.workflows.demo.phases[0].gate.channels=['slack'];assert.doesNotMatch(s.gateActions(job,gate),/data-decide/);
});
test('confirmation preserves reviewed digest across background refresh and requires review',async()=>{
  const {sandbox:s,element,requests}=setup();
  s.beginDecision('gate',true);s.changeDigest();
  await element('decision-form').onsubmit({preventDefault(){}});assert.equal(requests.length,0);
  element('decision-reviewed').checked=true;
  await element('decision-form').onsubmit({preventDefault(){}});
  const body=JSON.parse(requests[0].body);assert.equal(body.artifact_digest,'reviewed-digest');assert.equal(body.approve,true);assert.equal(body.gate_id,'gate');
});
test('uncertain submission retries reuse event identity and report errors',async()=>{
  const {sandbox:s,element,requests}=setup();
  s.beginDecision('gate',false);element('decision-reviewed').checked=true;
  s.fetch=async(url,options)=>{requests.push(options);throw Error('Connection lost')};
  await element('decision-form').onsubmit({preventDefault(){}});
  assert.equal(element('decision-error').hidden,false);assert.equal(element('submit-decision').disabled,false);
  await element('decision-form').onsubmit({preventDefault(){}});
  assert.equal(requests[0].body,requests[1].body);assert.equal(JSON.parse(requests[0].body).approve,false);
});

test('connector forms follow provider schema without redisplaying saved secrets',()=>{
  const nodes=new Map();
  const element=id=>{if(!nodes.has(id))nodes.set(id,{innerHTML:'',hidden:true,showModal(){this.open=true},addEventListener(){}});return nodes.get(id)};
  const s={$:element,document:{addEventListener(){}},access:{manage_connectors:true},accessVersion:0,state:{catalog:{repositories:[{id:'repo',provider:'github'},{id:'other',provider:'ado'}]}},escapeHTML:String};
  vm.createContext(s);
  vm.runInContext(source.slice(source.indexOf('let connectorCatalog ='),source.indexOf('let pendingLaunch =')),s);
  vm.runInContext(`connectorCatalog=[{definition:{kind:'github',name:'GitHub',description:'Repository access',fields:[{key:'token',label:'Access token',secret:true,required:true}]},revision:1,enabled:true,configured:true,repositories:['repo'],values:{token:'must-never-render'},secrets:{token:true}}]`,s);
  s.editConnector('github');
  assert.match(element('connector-fields').innerHTML,/type="password"/);
  assert.match(element('connector-fields').innerHTML,/leave blank to keep/);
  assert.doesNotMatch(element('connector-fields').innerHTML,/must-never-render/);
  assert.equal(nodes.has('connector-repositories'),false);
  s.access.manage_connectors=false;
  assert.match(s.connectors(),/administrator/);
  assert.doesNotMatch(s.connectors(),/data-connector=/);
});

test('connection test submits the draft without saving and restores controls',async()=>{
  const nodes=new Map(),requests=[];
  const field={dataset:{field:'token'},value:'',disabled:false};
  const control={disabled:false};
  const element=id=>{if(!nodes.has(id))nodes.set(id,{innerHTML:'',hidden:true,disabled:false,addEventListener(){},querySelectorAll(selector){return id==='connector-form'?[field,control]:id==='connector-fields'&&selector==='[data-field]'?[field]:[]}});return nodes.get(id)};
  const s={$:element,document:{addEventListener(){}},access:{manage_connectors:true},accessVersion:0,escapeHTML:String,date:String,label:String,sessionStorage:{getItem:()=> 'admin-token'},fetch:async(url,options)=>{requests.push({url,...options});return {ok:true,json:async()=>({ok:true,checked_at:'now',checks:[{name:'Primary credential',status:'passed',message:'Provider authenticated the credential.'}]})}}};
  vm.createContext(s);
  vm.runInContext(source.slice(source.indexOf('let connectorCatalog ='),source.indexOf('let pendingLaunch =')),s);
  vm.runInContext(`editingConnector={kind:'jira',revision:7,version:0}`,s);
  await element('connector-test').onclick();
  assert.equal(requests.length,1);
  assert.equal(requests[0].url,'/api/connectors/jira/test');
  assert.equal(requests[0].method,'POST');
  const body=JSON.parse(requests[0].body);
  assert.equal(body.revision,7);assert.equal(body.values.token,'');
  assert.match(element('connector-test-result').innerHTML,/Authentication checks passed/);
  assert.match(element('connector-test-result').innerHTML,/Settings were not saved/);
  assert.equal(control.disabled,false);
});

function launchSetup() {
  const nodes=new Map(), requests=[], opened=[];
  const element=id=>{if(!nodes.has(id))nodes.set(id,{value:'',hidden:true,disabled:false,innerHTML:'',addEventListener(){},querySelectorAll(){return []},showModal(){this.open=true},close(){this.open=false},reset(){}});return nodes.get(id)};
  let id=0;
  const s={$:element,access:{operable_repositories:['*']},accessVersion:0,state:{catalog:{workflows:{review:{id:'review',phases:[{inputs:{}}]},followup:{id:'followup',phases:[{inputs:{plan:'parent.artifacts.plan'}}]}}}},escapeHTML:String,crypto:{randomUUID:()=>`id-${++id}`},sessionStorage:{getItem:()=> 'operator-token'},location:{hash:''},short:String,toast(){},refresh:async()=>{},loadDetail:async id=>opened.push(id),fetch:async(url,options)=>{requests.push({url,...options});return {ok:true,json:async()=>({id:'created-job'})}}};
  vm.createContext(s);
  s.clearTimeout=clearTimeout; s.setTimeout=setTimeout;
  vm.runInContext(source.slice(source.indexOf('let pendingLaunch ='),source.indexOf('function render() {')),s);
  element('launch-workflow').value='review';element('launch-repository').value='https://github.com/team/new';element('launch-provider').value='manual';element('launch-task-title').value='Review this';
  return {s,element,requests,opened};
}
test('PR review needs only its URL and optional ticket, and preserves retry identity',async()=>{
  const {s,element,requests}=launchSetup();
  s.state.catalog.workflows['pr-review']={id:'pr-review',phases:[{inputs:{},tasks:[{uses:'pull_request.fetch'}]}]};
  element('launch-workflow').value='pr-review';
  element('launch-repository').value=''; element('launch-task-title').value='';
  element('launch-pr-url').value='https://github.com/o/r/pull/1';
  element('launch-ticket').value='TEAM-12';
  s.setLaunchMode();
  assert.equal(element('launch-task-fields').hidden,true);
  assert.equal(element('launch-task-title').required,false);
  assert.equal(element('launch-pr-url').required,true);
  assert.equal(element('launch-task-title').disabled,true);
  s.fetch=async(url,options)=>{requests.push({url,...options});throw Error('Connection lost')};
  for(let i=0;i<2;i++)await element('launch-form').onsubmit({preventDefault(){}});
  assert.equal(requests[0].url,'/api/pr-reviews');
  assert.deepEqual(JSON.parse(requests[0].body),{workflow:'pr-review',pr_url:'https://github.com/o/r/pull/1',ticket:'TEAM-12'});
  assert.equal(requests[0].headers['Idempotency-Key'],requests[1].headers['Idempotency-Key']);
  element('launch-workflow').value='review';s.setLaunchMode();
  assert.equal(element('launch-task-fields').hidden,false);
  assert.equal(element('launch-task-title').required,true);
  assert.equal(element('launch-pr-url').disabled,true);
});
test('portal launch excludes followups and opens submitted job using operator credentials',async()=>{
  const {s,element,requests,opened}=launchSetup();
  s.openLaunch();
  assert.match(element('launch-workflow').innerHTML,/review/);
  assert.doesNotMatch(element('launch-workflow').innerHTML,/followup/);
  await element('launch-form').onsubmit({preventDefault(){}});
  assert.equal(requests[0].url,'/api/jobs');assert.equal(requests[0].headers.Authorization,'Bearer operator-token');
  assert.equal(JSON.parse(requests[0].body).repository,'https://github.com/team/new');
  assert.equal(JSON.parse(requests[0].body).issue.key,'manual-id-1');
  assert.deepEqual(opened,['created-job']);
});
test('portal launch reuses payload and key after lost response, but changes key for edited request',async()=>{
  const {s,element,requests}=launchSetup();
  s.fetch=async(url,options)=>{requests.push(options);throw Error('Connection lost')};
  for(let i=0;i<2;i++)await element('launch-form').onsubmit({preventDefault(){}});
  assert.equal(requests[0].body,requests[1].body);
  assert.equal(requests[0].headers['Idempotency-Key'],requests[1].headers['Idempotency-Key']);
  assert.equal(element('launch-error').hidden,false);
  element('launch-task-title').value='Changed request';
  await element('launch-form').onsubmit({preventDefault(){}});
  assert.notEqual(requests[0].headers['Idempotency-Key'],requests[2].headers['Idempotency-Key']);
});
test('portal launch rejects unauthorized repositories and invalid PR inputs',async()=>{
  const {s,element,requests}=launchSetup();
  s.access.operable_repositories=['other'];
  await element('launch-form').onsubmit({preventDefault(){}});
  assert.equal(requests.length,0);assert.match(element('launch-error').textContent,/operator access/);
  s.access.operable_repositories=['*'];element('launch-provider').value='github_pr';element('launch-key').value='abc';
  await element('launch-form').onsubmit({preventDefault(){}});
  assert.equal(requests.length,0);assert.match(element('launch-error').textContent,/positive integer/);
});
test('URL repository approvals use identity scopes while alias maintainers remain required',()=>{
  const {sandbox:s}=setup();
  vm.runInContext("access.dynamic_approvable_repositories=['*'];access.approvable_repositories=[]",s);
  assert.equal(s.canApproveRepository('https://github.com/team/new'),true);
  assert.equal(s.canApproveRepository('repo'),false);
});

test('Jira lookup waits for two characters, debounces, selects and ignores stale responses',async()=>{
  const {s,element,requests}=launchSetup();
  let scheduled=null;
  s.clearTimeout=()=>{scheduled=null}; s.setTimeout=fn=>{scheduled=fn;return 1};
  element('launch-key').focus=()=>{};
  element('launch-provider').value='jira';
  element('launch-key').value='A';s.queueJiraSearch();assert.equal(scheduled,null);
  element('launch-key').value='AB';s.queueJiraSearch();assert.ok(scheduled);
  s.fetch=async(url,options)=>{requests.push({url,...options});return {ok:true,json:async()=>({issues:[{key:'AB-1',summary:'Example ticket'}]})}};
  await scheduled();
  assert.equal(requests[0].url,'/api/jira/issues?query=AB');
  assert.equal(requests[0].headers.Authorization,'Bearer operator-token');
  assert.match(element('jira-results').innerHTML,/AB-1/);
  s.fetch=async()=>({ok:true,json:async()=>({description:{type:'doc',content:[{type:'paragraph',content:[{type:'text',text:'Ticket description'}]}]}})});
  await element('jira-results').onclick({target:{closest:()=>({dataset:{jiraIndex:'0'}})}});
  assert.equal(element('launch-key').value,'AB-1');assert.equal(element('launch-task-title').value,'Example ticket');
  assert.equal(element('launch-body').value,'Ticket description');
  assert.equal(s.jiraDescriptionText(null),'');
  assert.equal(s.jiraDescriptionText('Plain description'),'Plain description');
  element('launch-key').value='AB';s.queueJiraSearch();
  let resolve; s.fetch=()=>new Promise(r=>resolve=r);
  const pending=scheduled();
  element('launch-key').value='A';s.queueJiraSearch();
  resolve({ok:true,json:async()=>({issues:[{key:'OLD-1',summary:'Stale'}]})});await pending;
  assert.equal(element('jira-results').innerHTML,'');
  element('launch-provider').value='manual';s.queueJiraSearch();assert.equal(element('jira-search').hidden,true);
});

test('Jira details do not overwrite a newer selection or user edits',async()=>{
  const {s,element}=launchSetup();
  element('launch-key').focus=()=>{};
  vm.runInContext("jiraMatches=[{key:'AB-1',summary:'First'}]",s);
  let resolve;s.fetch=()=>new Promise(r=>resolve=r);
  const pending=element('jira-results').onclick({target:{closest:()=>({dataset:{jiraIndex:'0'}})}});
  element('launch-body').value='User edit';
  resolve({ok:true,json:async()=>({description:'Remote description'})});await pending;
  assert.equal(element('launch-body').value,'User edit');
  vm.runInContext("jiraMatches=[{key:'AB-2',summary:'Second'}]",s);
  const stale=element('jira-results').onclick({target:{closest:()=>({dataset:{jiraIndex:'0'}})}});
  s.clearJiraSearch();
  resolve({ok:true,json:async()=>({description:'Stale description'})});await stale;
  assert.equal(element('launch-body').value,'');
});

test('Jira field editor saves IDs and headings in order and previews the draft',async()=>{
  const nodes=new Map(),requests=[];
  const element=id=>{if(!nodes.has(id))nodes.set(id,{value:'',hidden:true,innerHTML:'',textContent:'',addEventListener(){},querySelectorAll(){return []}});return nodes.get(id)};
  const s={$:element,document:{addEventListener(){}},accessVersion:0,escapeHTML:String,sessionStorage:{getItem:()=> 'admin'},fetch:async(url,options)=>{requests.push({url,...options});return {ok:true,json:async()=>url.endsWith('/fields')?{fields:[{id:'customfield_12',name:'Acceptance Criteria'},{id:'customfield_13',name:'Context'}]}:{description:'Combined text',field_statuses:[{heading:'Acceptance criteria',status:'included'}]}}}};
  vm.createContext(s);vm.runInContext(source.slice(source.indexOf('let connectorCatalog ='),source.indexOf('let pendingLaunch =')),s);
  vm.runInContext("editingConnector={kind:'jira',revision:3,version:0}",s);
  s.setupJiraFields({definition:{kind:'jira'},values:{}});
  await element('jira-load-fields').onclick();
  assert.match(element('jira-field-choice').innerHTML,/Acceptance Criteria/);
  element('jira-field-choice').value='customfield_12';element('jira-add-field').onclick();
  element('jira-field-filter').value='acceptance';s.filterJiraFields();
  assert.match(element('jira-field-choice').innerHTML,/Acceptance Criteria.*Already included/);
  assert.match(element('jira-field-choice').innerHTML,/disabled/);
  element('jira-field-filter').value='';s.filterJiraFields();
  element('jira-field-choice').value='customfield_13';element('jira-add-field').onclick();
  element('jira-field-rows').oninput({target:{dataset:{jiraHeading:'0'},value:'Acceptance criteria'}});
  element('jira-field-rows').onclick({target:{closest:selector=>selector==='[data-jira-move]'?{dataset:{jiraMove:'1',direction:'-1'}}:null}});
  const fields=JSON.parse(s.connectorPayload().values.issue_fields);
  assert.deepEqual(fields,[{id:'customfield_13',heading:'Context'},{id:'customfield_12',heading:'Acceptance criteria'}]);
  element('jira-preview-key').value='SCRUM-1';await element('jira-preview').onclick();
  assert.equal(requests[1].url,'/api/connectors/jira/preview/SCRUM-1');
  assert.equal(JSON.parse(requests[1].body).revision,3);
  assert.equal(element('jira-field-preview').textContent,'Combined text');
  assert.match(element('jira-field-message').textContent,/included/);
  element('jira-field-rows').onclick({target:{closest:selector=>selector==='[data-jira-remove]'?{dataset:{jiraRemove:'0'}}:null}});
  assert.equal(JSON.parse(s.connectorPayload().values.issue_fields).length,1);
});

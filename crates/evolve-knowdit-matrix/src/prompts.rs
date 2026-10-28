//! Every prompt of the matrix stages. Rendering functions take the already-computed facts
//! (scope, overview, inventory, drafts, rows) and produce the system / user prompt pair of one
//! agent run, including the exact payload shape the run's submit tool expects.

use evolve_knowdit_project::ExtentFunction;
use evolve_knowdit_types::{
    CellRef, CoverageMatrix, Dimension, InputKind, MatrixDraft, Property, ScopeContext,
    SourceAnchor, Subject, VerificationTask,
};

/// The audit model as every matrix agent sees it. Fixed text; the protocol-specific part is
/// rendered separately.
pub struct AuditModel;

impl AuditModel {
    pub fn threat_model() -> String {
        "## Adversary and impact (governs every stage)\n\
         The audit targets vulnerabilities that an unprivileged external caller can exploit: an account that holds no owner, admin, governor, operator, keeper or other trusted role, possibly using contracts it deploys itself, possibly with several accounts, possibly with flash-loaned funds. The impact must harm the protocol or other users: loss or lock of funds, corrupted accounting, broken invariants, denial of service against others, or value extracted at others' expense.\n\
         Out of scope: centralisation and privileged-role abuse, governance design, harm a user can only do to themselves, and hypothetical future misconfiguration by an admin. A privileged action stays in scope only when the code itself lacks a constraint that protects other users from it.\n\
         Everything an audit report or a known-issues list already declares as known, accepted or out of scope is excluded.".to_string()
    }

    pub fn matrix_definition() -> String {
        "## The coverage matrix\n\
         The audit models the blockchain at three levels and reads the protocol at each: the transaction level yields operations, the contract level yields modules, the asset level yields assets. Every operation, module and asset is a subject: a row of one coverage matrix. Every property any level's reading produces is a column of that same matrix. A cell (subject × property) carries an obligation: the property instantiated for that subject, which a later stage must decide against an unprivileged external caller. A cell whose property cannot apply to the subject is excluded and rendered `-`. The task stage states each obligation as a verification task whose open inputs a Foundry test tries to move into the violation.\n\
         The three readings overlap on purpose: the same defect may surface as an operation settling wrong, as a module breaking its invariant and as an asset losing conservation. The matrix keeps all three rows so that no reading's blind spot hides it.\n\
         Properties come in two shapes and both are needed: safety (a state the protocol must never reach) and availability (an action the protocol promises to an entitled party — a later adjustment, a claim, an exit, an unregistration — that must stay possible; its violation is that action reverting or being wrongly refused after ordinary prior state, whether reached by the party itself or set up by an unprivileged third party).".to_string()
    }

    pub fn dimension_definition(dimension: Dimension) -> String {
        match dimension {
            Dimension::OperationFlow => {
                "## Level: Operation Flow (transaction level)\n\
                A transaction is atomic and open to any caller. The subject is one user-triggerable operation: an entry point an unprivileged caller can invoke, read from that entry point through every contract it reaches until the transaction settles. Thin wrappers and variants that share one settlement path form one subject; entry points with different settlement paths are separate subjects.\n\
                A property this reading yields states what an operation settles into: a condition on the committed post-state that must hold whenever the operation succeeds, and a condition on what the operation must refuse. Shapes to derive concrete ones from: the accounting the operation writes matches the value it moved; the operation applies only to positions the caller is entitled to act on; the operation cannot be replayed, front-run or reordered into a settlement the protocol did not intend; the operation cannot leave the protocol insolvent or a counterparty unpaid; the operation's callbacks and external calls cannot re-enter into an inconsistent intermediate state; the operation's arithmetic rounds against the caller."
            }
            Dimension::Module => {
                "## Level: Module (contract level)\n\
                A contract owns its storage and interface and any other contract may invoke it. The subject is one unit (contract, abstract contract or library), read together with all of its callers and everything it inherits.\n\
                A property this reading yields states a guarantee the unit's interface makes to the rest of the system: an invariant over its storage that every entry point preserves; who may call which entry point; the relation between the unit's derived views and its stored state; what the unit assumes about the contracts it calls and what it guarantees to the contracts that call it; safe initialization and upgrade state."
            }
            Dimension::AssetLifecycle => {
                "## Level: Asset Lifecycle (asset level)\n\
                An asset persists across transactions and contracts. The subject is one asset or accounting variable (token balances the protocol holds, shares and their supply, debt and collateral positions, reward accruals, fee accumulators, escrowed amounts, per-user ledgers), read across every path that creates, moves, prices or destroys it.\n\
                A property this reading yields states an accounting consistency across the asset's lifecycle: the sum of per-user records equals the total; claims on the asset never exceed what backs them; nothing is created without backing or destroyed without settlement; pricing the asset uses inputs an unprivileged caller cannot steer within one transaction; ownership and entitlement of the asset move only with the transactions the protocol intends.\n\
                The asset's lifecycle includes the real behaviour of the token contract that carries it: the mainstream tokens deployed today deviate from the ERC20 text in known ways (transfer and approve without a return value, six decimals, approval-race protection), and the protocol's own token list or README decides which tokens, and therefore which deviations, are in scope."
            }
        }
        .to_string()
    }
}

/// Protocol-specific facts rendered into every matrix prompt.
pub struct MatrixPromptContext {
    pub scope: ScopeContext,
    pub overview: String,
    pub tool_guide: String,
}

impl MatrixPromptContext {
    /// The engagement's own threat model when it gives one, else the built-in one.
    fn threat_model(&self) -> String {
        self.scope
            .threat_model
            .clone()
            .unwrap_or_else(AuditModel::threat_model)
    }

    fn render_scope(&self) -> String {
        let mut out = String::from("## Engagement scope\n");
        if self.scope.in_scope.is_empty() {
            out.push_str(
                "No explicit scope list; every indexed project source file is in scope.\n",
            );
        } else {
            out.push_str("In-scope files:\n");
            for f in &self.scope.in_scope {
                out.push_str(&format!("- {f}\n"));
            }
        }
        if !self.scope.out_of_scope.is_empty() {
            out.push_str(
                "Explicitly out-of-scope files (readable for context, never a subject):\n",
            );
            for f in self.scope.out_of_scope.iter().take(60) {
                out.push_str(&format!("- {f}\n"));
            }
        }
        if !self.scope.readme_excerpt.is_empty() {
            out.push_str("\n## Project README (excerpt; trust assumptions, roles and declared exclusions apply)\n");
            out.push_str(&self.scope.readme_excerpt);
            out.push('\n');
        }
        if !self.scope.known_issues.is_empty() {
            out.push_str("\n## Known issues declared by the project (excluded from the audit)\n");
            out.push_str(&self.scope.known_issues);
            out.push('\n');
        }
        out
    }

    fn render_subject(subject: &Subject) -> String {
        format!(
            "{} `{}` ({} level) — {} [anchors: {}]",
            subject.id,
            subject.name,
            subject.dimension.level(),
            subject.summary,
            subject
                .anchors
                .iter()
                .map(|a| a.to_string())
                .collect::<Vec<_>>()
                .join("; ")
        )
    }

    // ---- stage 1: one level's draft -------------------------------------------------------

    pub fn draft_system(&self, dimension: Dimension, max_properties: usize) -> String {
        format!(
            "You are the lead auditor reading the protocol under audit at one level of the blockchain model: this run produces the {title} draft of the coverage matrix. You read the source with the tools, then call `submit_draft` once with the complete draft. The submission is validated: every problem it reports must be fixed and the draft resubmitted until it is accepted.\n\n\
             {threat}\n\n{matrix}\n\n{definition}\n\n\
             ## What this level's draft contains\n\
             - `subjects`: the rows this level contributes. Enumerate them from the inventory in the user message: every inventory entry must be an anchor of some subject or be listed under `subject_exclusions` with a reason grounded in source (pure view without state change, thin wrapper of a modeled entry point, configuration constant, ...). Group inventory entries into one subject only when they share one extent and one settlement path. Subject ids carry the level code: `{code}01`, `{code}02`, ...\n\
             - `properties`: the candidate columns this level's reading yields. Read them from the source, not from a checklist: each property names a guarantee this protocol makes to a party (its users, its counterparties, the contracts that call it, and the parties it pays: the protocol's own treasury, its liquidity providers, fee recipients such as builders or referrers), stated as a checkable condition whose violation is an observable post-state. A property is a class of guarantee, not one contract's invariant: word it so it can be asked of every subject that moves the same kind of value, grants the same kind of entitlement or makes the same kind of promise, whichever contract or level it lives in. Make it specific in what it compares (the accounting relation, the entitlement, the ordering, the pricing input or the promised action it protects), not in which contract it names.\n\
             - Read what the code does from the source; never take what the code does as the standard of what is correct. A property compares the post-state with a reference independent of the implementation under audit: the funds actually moved in and out, the time actually elapsed, what a signer actually approved, the real behaviour of the external systems it uses, what the README or NatSpec promises. A value the code itself computes (its snapshot, its view, its derived total) or the set of checks it performs is never that reference: the defect may live in exactly that computation or that check. A property about a guard or an access check states what the guard protects (the asset or action it constrains), not which checks it performs. Test every property before submitting it: if the implementation got this computation or this check wrong, would the property still hold? If yes, it restates the implementation and cannot be violated; rewrite it against the independent reference.\n\
             - Keep properties orthogonal: two properties whose violations are the same post-state are one property.\n\
             - Include the availability side: for every action this level exposes that later ordinary state could refuse — a claim, an exit, a cancelation, a later adjustment, a registry removal, an unregistration — propose a property stating that the action stays possible for the entitled party, whose violation is that action reverting after state an unprivileged party can create or leave behind (dust, a pending request, a stale record). A protocol that exposes such actions yields at least one availability property.\n\
             - Include the full-payment side: for every amount the protocol owes or charges a party — a fee or commission owed to the protocol, its liquidity providers or a fee recipient, interest owed to lenders, premium owed to sellers, a reward — propose a property stating that the party receives, or is charged, the full amount the documented rule (README, NatSpec, the fee or rate schedule) sets for the action, whatever route or order the action takes. Its violation is the party receiving less than that rule's amount, or two routes to the same outcome (a direct action against the same action split, batched, or preceded by another settlement) paying different amounts. A protocol that charges fees or owes interest, premium or rewards yields at least one full-payment property.\n\
             - Subjects and anchors must point at indexed project files; use the exact relative paths and names the tools report.\n\n\
             ## Limits\n- at most {max_properties} candidate properties; typically 4 to 8.\n- Scan every in-scope file before submitting. Read the entry points and the storage layout; do not plan from names alone.\n- The `scope_snapshot` is two to five sentences: what the protocol does and which in-scope code this level's reading covers.\n\n\
             {scope}\n\n## Project overview\n{overview}\n\n{tools}\n\n\
             Finish by calling `submit_draft`. Do not describe the draft in prose instead of submitting it.",
            title = dimension.title(),
            code = dimension.code(),
            threat = self.threat_model(),
            matrix = AuditModel::matrix_definition(),
            definition = AuditModel::dimension_definition(dimension),
            scope = self.render_scope(),
            overview = self.overview,
            tools = self.tool_guide,
        )
    }

    pub fn draft_user(&self, dimension: Dimension, inventory: &str) -> String {
        let code = dimension.code();
        let slug = dimension.slug();
        let anchor = match dimension {
            Dimension::Module => r#"{"file": "src/Vault.sol", "contract": "Vault"}"#,
            Dimension::OperationFlow => {
                r#"{"file": "src/Vault.sol", "contract": "Vault", "function": "withdraw"}"#
            }
            Dimension::AssetLifecycle => {
                r#"{"file": "src/Vault.sol", "contract": "Vault", "state": "totalShares"}"#
            }
        };
        format!(
            "Produce the {title} draft for this protocol.\n\n\
             ## Source inventory for this level (every entry must be anchored by a subject or excluded with a reason)\n{inventory}\n\n\
             ## Payload shape of `submit_draft`\n```json\n{{\n  \"dimension\": \"{slug}\",\n  \"scope_snapshot\": \"two to five sentences\",\n  \"subjects\": [\n    {{\"id\": \"{code}01\", \"dimension\": \"{slug}\", \"name\": \"short name\", \"summary\": \"what it is and its extent\", \"anchors\": [{anchor}]}}\n  ],\n  \"properties\": [\n    {{\"id\": \"P01\", \"origin\": \"{slug}\", \"name\": \"short name\", \"statement\": \"checkable condition whose violation is an observable post-state\"}}\n  ],\n  \"subject_exclusions\": [\n    {{\"anchor\": {anchor}, \"reason\": \"why no subject models this inventory entry\"}}\n  ]\n}}\n```\n\
             `dimension` and every `origin` are exactly \"{slug}\"; subject ids are {code}01, {code}02, ...; `anchors` and `subject_exclusions` are arrays; anchor keys are `file`, `contract`, `function`, `state`.\n\n\
             Read the code first, then submit through `submit_draft`.",
            title = dimension.title(),
        )
    }

    // ---- stage 2a: the merged columns -----------------------------------------------------

    pub fn properties_system(&self, max_properties: usize) -> String {
        format!(
            "You are the senior auditor who fixes the columns of the single coverage matrix of the protocol under audit. Three level drafts each proposed candidate properties; you merge them into one orthogonal set and submit it through `submit_properties`. Use the tools whenever a candidate leaves a doubt about the source.\n\n\
             {threat}\n\n{matrix}\n\n\
             ## How to merge\n\
             1. Two candidates whose violations are the same post-state become one property; keep the sharper statement and set `origin` to the level whose reading produced it. Two candidates that state the same guarantee for different components (the same accounting relation, entitlement, ordering or promised action in different contracts) also become one property, stated as the relation without naming the components.\n\
             2. Drop a candidate only when another property subsumes it. Every guarantee the protocol makes to a party that the candidates missed becomes a property too, including availability properties (promised actions that must stay possible). Check five families the level readings tend to skip and add the missing ones when the source has the feature: for every signature-authorised action, that the signed message binds the signer, every parameter the action uses, a nonce and a validity window; for every action that iterates (over records, or up to a stored maximum that grows with the pool), that the iteration stays bounded by the protocol rather than by what an unprivileged party can accumulate; for every value transfer out or callback, that the recipient's code runs only after the protocol's state is consistent; for every random draw, that one seed serves one draw and a delivery is bound to its request; for every privileged setter, that a cycle already opened (drawing, epoch, round, order, request) settles under the parameters it was opened with and can still complete after the setter runs; for every flag or mode bit a decoder reads, that both values are enforced and an unset bit skips no validation stage; for every batch executor, that a failed batch leaves no per-element signature or nonce usable for a subset; for every operation that moves value in and then prices shares, that the price excludes the operation's own inflow; for every amount the protocol owes or charges a party (fees, commissions, interest, premium, rewards), that the party receives or is charged the full amount its documented rule sets, on every route and in every order that performs the action.\n\
             3. Each final statement must be checkable for any subject it may be asked of: say what is compared with what, and what its violation looks like as a post-state. Do not restrict a property to the level that proposed it, nor to one contract or component: a column a single subject can be asked is that subject's invariant, not a column of the matrix.\n\
             4. The reference of every statement is independent of the implementation under audit: the funds actually moved, the time actually elapsed, what a signer actually approved, the real behaviour of external systems, what the README or NatSpec promises; never a value the code computes (its snapshot, its view, its derived total) or the checks it performs, because the defect may live there. A guard's property states what the guard protects, not its checks. Ask of every statement: if the implementation got this computation or check wrong, would the property still hold? If yes, it restates the implementation; rewrite it against the independent reference.\n\
             5. Number the properties `P01`, `P02`, ... in order; at most {max_properties}.\n\
             6. The `scope_snapshot` is two to five sentences for the whole protocol.\n\n\
             {scope}\n\n## Project overview\n{overview}\n\n{tools}\n\n\
             Finish by calling `submit_properties`.",
            threat = self.threat_model(),
            matrix = AuditModel::matrix_definition(),
            scope = self.render_scope(),
            overview = self.overview,
            tools = self.tool_guide,
        )
    }

    pub fn properties_user(&self, drafts: &[MatrixDraft]) -> String {
        let mut out = String::from(
            "Merge the candidate properties of these drafts into the final columns.\n",
        );
        for draft in drafts {
            out.push_str(&format!(
                "\n## {} draft ({} level)\n{}\n\n### Subjects of this level\n",
                draft.dimension.title(),
                draft.dimension.level(),
                draft.scope_snapshot
            ));
            for s in &draft.subjects {
                out.push_str(&format!("- {}\n", Self::render_subject(s)));
            }
            out.push_str("\n### Candidate properties\n");
            for p in &draft.properties {
                out.push_str(&format!(
                    "- [{}] {} `{}`: {}\n",
                    draft.dimension.slug(),
                    p.id,
                    p.name,
                    p.statement
                ));
            }
        }
        out.push_str(
            "\n## Payload shape of `submit_properties`\n```json\n{\n  \"scope_snapshot\": \"two to five sentences about the whole protocol\",\n  \"properties\": [\n    {\"id\": \"P01\", \"origin\": \"asset_lifecycle\", \"name\": \"short name\", \"statement\": \"what is compared with what, and what its violation looks like as a post-state\"}\n  ]\n}\n```\n`origin` is one of module, operation_flow, asset_lifecycle.\n\nThen call `submit_properties`.",
        );
        out
    }

    // ---- stage 2b: one subject's row -------------------------------------------------------

    pub fn row_system(&self) -> String {
        format!(
            "You are the auditor who decides one row of the single coverage matrix of the protocol under audit: for one subject and every property column, either the obligation the property becomes for that subject, or an exclusion. You read the subject's code with the tools (its anchors, what calls it, what it calls, the state it writes), then call `submit_row` once. The submission is validated: every property must be decided exactly once, and every problem it reports must be fixed and the row resubmitted until it is accepted.\n\n\
             {threat}\n\n{matrix}\n\n\
             ## What an obligation statement is\n\
             The property instantiated for this subject: name the functions and state variables of this subject that the property binds, and state the concrete post-state that counts as its violation, in terms a Foundry test could assert (balances, supplies, positions, roles, request states, a call reverting that must succeed). One or two sentences; never a copy of the generic property text.\n\
             State the obligation from what the protocol promises (the property, the README, the NatSpec), never from what the code happens to do: when a check anchors to one record while the operation concerns another (the original record instead of the effective one, the stored value instead of the input, the previous step instead of the latest), the obligation names the record the promise concerns and the code's choice is left for the task stage to break. An obligation that restates the implementation cannot be violated and hides the defect.\n\
             For an availability property the obligation names the action of this subject that must stay possible, who is entitled to it, and the state an unprivileged party could leave behind that must not block it; its violation is that action reverting.\n\
             ## Anchors\n\
             Every obligation lists `anchors`: the functions of this subject through which an unprivileged sequence could reach the violation post-state (the entry points and the writers of the state the post-state reads), each as {{file, contract, function}}, plus the state variables it reads as {{file, contract, state}}. Anchor every function that belongs: the list in the user message is a starting point, not a limit, and the task stage walks exactly these functions for this obligation, so a function left out here is a function no task will test against it. Copy names from the list or from the code; an inherited function is named on the contract that exposes it. An anchor outside the list is kept when the obligation's `reach` states the call path from it into this subject's code or state (which entry point calls which function of the subject, or which function writes the state the post-state reads); leave `reach` empty when every anchor is listed.\n\
             ## When to exclude\n\
             Only when the property is meaningless for this subject: the subject holds no such asset, performs no such settlement, has no such entitlement or promised action. Never because it looks safe, never because a guard exists (a guard is what the task stage will try to get around), and never because another row already covers it: the same defect being owed by an operation, a module and an asset is intended. Give the reason from the source.\n\n\
             {scope}\n\n## Project overview\n{overview}\n\n{tools}\n\n\
             Finish by calling `submit_row`.",
            threat = self.threat_model(),
            matrix = AuditModel::matrix_definition(),
            scope = self.render_scope(),
            overview = self.overview,
            tools = self.tool_guide,
        )
    }

    pub fn row_user(
        &self,
        subject: &Subject,
        properties: &[Property],
        scope_snapshot: &str,
        functions: &[ExtentFunction],
    ) -> String {
        let mut out = format!(
            "Decide the row of subject {}.\n\n## Protocol snapshot\n{scope_snapshot}\n\n## Subject\n{}\n\n## Properties (decide each one)\n",
            subject.id,
            Self::render_subject(subject)
        );
        for p in properties {
            out.push_str(&format!(
                "- {} `{}` [{}]: {}\n",
                p.id,
                p.name,
                p.origin.slug(),
                p.statement
            ));
        }
        out.push_str("\n## Functions of this subject (a starting list, not a limit: its contracts, their bases and derived contracts, the writers and readers of its state, one hop of calls, and the entry points that reach them; copy `contract` and `function` from here or from the code)\n");
        let mut last_contract = String::new();
        for function in functions.iter().filter(|f| !f.read_only) {
            if function.contract != last_contract {
                last_contract = function.contract.clone();
                out.push_str(&format!("\n### {}\n", function.contract));
            }
            out.push_str(&format!("- {function}\n"));
        }
        let read_only: Vec<&ExtentFunction> = functions.iter().filter(|f| f.read_only).collect();
        if !read_only.is_empty() {
            out.push_str("\n### Read-only functions\n");
            for function in read_only {
                out.push_str(&format!("- {function}\n"));
            }
        }
        out.push_str(&format!(
            "\n## Payload shape of `submit_row`\n```json\n{{\n  \"subject_id\": \"{}\",\n  \"obligations\": [\n    {{\"property_id\": \"P01\", \"statement\": \"the property instantiated for this subject: the functions and state it binds and the post-state that is its violation\", \"anchors\": [{{\"file\": \"src/Vault.sol\", \"contract\": \"Vault\", \"function\": \"withdraw\"}}, {{\"file\": \"src/Vault.sol\", \"contract\": \"Vault\", \"state\": \"totalAssets\"}}], \"reach\": \"\"}}\n  ],\n  \"exclusions\": [\n    {{\"property_id\": \"P02\", \"reason\": \"why the property is meaningless for this subject, from the source\"}}\n  ]\n}}\n```\nEvery property id appears exactly once, in `obligations` or in `exclusions`; every obligation has at least one anchor naming a function; an obligation anchoring a function or state outside the list above states in `reach` the call path from each such anchor into this subject.\n\nRead the subject's code, then call `submit_row`.",
            subject.id
        ));
        out
    }

    // ---- stage 3: the task of one obligation cell -----------------------------------------

    pub fn tasks_system(&self) -> String {
        format!(
            "You are the auditor who writes the task of one cell of the coverage matrix of the protocol under audit. The cell is a property instantiated for a subject, to be decided against an unprivileged external caller. You read the function through which that obligation can be violated and write its ledger: what its execution consumes, what its checks bind. A unit agent then writes the reproductions that move the inputs no check binds; a later stage runs each reproduction on its own in a Foundry test compiled against this repository, and one that reaches the cell's violating post-state becomes a finding.\n\n\
             {threat}\n\n{matrix}\n\n\
             ## What a task is\n\
             - The task is the ledger of ONE function against this cell's obligation: `violation` (the post-state, over project state, that shows the obligation failing), `contract` and `function` (the function through which an unprivileged external caller reaches that post-state, copied from the listing; usually an entry point the obligation names), a framing `claim` (`an unprivileged external caller can reach <the violation> through <function>`), `consumes`, `binds`.\n\
             - `consumes` is every value, identity, record and moment the function's execution reads to decide its effect, one entry each, from the body: the amounts it moves, the recipients it pays, the values it forwards, the prices and rates it reads, the records it writes or reads, the configuration and addresses read at execution, `msg.value`. Each entry carries `kind` — `parameter` (a call argument), `value` (msg.value), `storage` (read before the checks or the effect, written elsewhere), `external` (returned by a callee, or read after an external call), `authorization` (a field a signature, permit, approval or allowlist is meant to cover), `configuration` (a role can change it), `moment` (block, timestamp, epoch boundary, ordering), `record` (one entry selected among many by key or index), `constant` (a value the code fixes: a constant, an immutable, a literal bound; never open, so it needs no `binds` entry) — and `decided_by`: who decides its value as the function runs (`the caller`, `the signer`, `the owner`, `the callee <contract>`, `storage written by <entry point>`, `the block`, `a constant`). A `consumes` of two entries for a function that moves funds is a misread.\n\
             - `binds` is the subset of `consumes` a check FIXES: `input` (one entry of `consumes`, copied exactly), `check` (what it requires and where: `Contract.function`, file:line) and `fixes` (what the value is fixed to, and so who decided it). A value is fixed when, after the check, the unprivileged caller no longer chooses it: it equals `msg.sender`, it is a field the signer signed, it is the caller's own balance or allowance or position, it is a stored entitlement of the acting account, a constant, an owner-set parameter. Excluding one value (`!= 0`, `!= address(0)`) is NOT fixing it: the caller still chooses every other value. Bounding it (`<= cap`, `>= min`, a range) is NOT fixing it unless the bound is the caller's own entitlement: the caller still chooses within the bound, and a reproduction moves it to the edge or to the value the check never compared. Ask of every consumed value: who decided it? When the answer is `the caller`, it is open, whatever `require` touched it on the way.\n\
             - Every entry of `consumes` that no `binds` entry names is an open input. You do not write reproductions for them: when the task is accepted a unit agent writes the scenarios that move every open input, and a reproduction agent builds each scenario as a Foundry test and observes the cell's promise. Your ledger decides what gets verified; the kind and the decider tell the scenarios how.\n\
             - There is no third disposition. An input is bound by a named check or it is moved by a reproduction. `The caller cannot profit`, `standard pattern`, `holds by inspection` are not entries of the ledger; when you believe a guard closes the path, the reproduction tries the paths around that guard and the dynamic stage decides.\n\
             ## How to read a function into its ledger\n\
             Read the body, not the name. The user message lists the code the obligation names and the functions you may choose from; read the one the violation goes through, and the functions it calls. Answer these for it, in its `consumes` and `binds`; they are how to read, not what to find, and the answers are yours to draw from the code and from what you know about deployed contracts.\n\
             1. Whom does it call and what does it assume about them? For every external call (a token, an oracle, a bridge, a callback, a hook) list what the code takes for granted about the callee — a return value that exists and is true, no revert, the amount moved equalling the argument, no re-entry, no state of this contract changed during the call — and whether widely deployed implementations violate any of those.\n\
             2. Which configuration and optional components does it read, and in which state? Every address, flag or parameter read from storage, in the configured and the unset state, and whether the caller chooses which branch runs.\n\
             3. Where do the numbers it compares come from, and against what? For every comparison, `min`/`max`, division or cast: who decides each operand, can a caller drive it to zero or to an extreme, is the bound the protocol's own or one the chain imposes.\n\
             4. What does it write, in which order, and who can come back in? Whether state is written before or after external calls, which other entry points write the same state, which writes survive a failure part-way.\n\
             5. Who can call it and on whose behalf? What the authorisation rests on (msg.sender, a signature, an allowance, a role), which fields a signature or approval binds and which it leaves free, whether it can be replayed.\n\
             6. Does its promised effect hold at the extremes? Zero and maximal values, repeated calls in one block, partial failure inside a batch, a position or record that is the only one or the last one.\n\
             Each answer is either a `binds` entry (a check fixes it, with `fixes` saying to what) or an open input (nothing does), whose `kind` and `decided_by` tell the scenarios how it will be moved.\n\
             ## Rules\n\
             - Privileged triggers: a consumed input that is a configuration a trusted role may change at that moment is of kind `configuration`, decided by that role; its reproduction has the role change it between the check and the effect and an unprivileged participant act afterwards. In scope whenever the code itself lacks the constraint that would protect them.\n\
             - Stay within the adversary model: a sequence uses no privileged role beyond that and no cheat on project storage; setup may use a trusted role only to create a realistic state that the protocol's own deployment or normal operation would create.\n\
             - Stay on the protocol's real chain: precompiles, system contracts, oracles and standard tokens behave as they do in production; a dependency's real deviations (a token with no return value or six decimals) count, its misbehaviour does not.\n\
             - Anything the README or known issues declare as accepted is bound by that declaration: name it as the check.\n\
             - Read the code: confirm every function you name exists with the signature you rely on, and follow the calls it makes.\n\n\
             {scope}\n\n## Project overview\n{overview}\n\n{tools}\n\n\
             Read the function, then call `submit_task` with this cell's task. A rejected submission names the problems; fix them and submit again.",
            threat = self.threat_model(),
            matrix = AuditModel::matrix_definition(),
            scope = self.render_scope(),
            overview = self.overview,
            tools = self.tool_guide,
        )
    }

    pub fn tasks_user(
        &self,
        matrix: &CoverageMatrix,
        cell: &CellRef,
        statement: &str,
        anchors: &[SourceAnchor],
        functions: &[ExtentFunction],
    ) -> String {
        let mut out = format!("Write the task of cell {cell}.\n\n");
        if let Some(s) = matrix.subjects.get(&cell.subject_id) {
            out.push_str(&format!("## Subject\n{}\n\n", Self::render_subject(s)));
        }
        if let Some(p) = matrix.properties.get(&cell.property_id) {
            out.push_str(&format!("## Property\n{} `{}`: {}\n\n", p.id, p.name, p.statement));
        }
        out.push_str(&format!("## Obligation\n{statement}\n"));
        if !anchors.is_empty() {
            out.push_str(&format!(
                "It names: {}\n",
                anchors.iter().map(|a| a.to_string()).collect::<Vec<_>>().join(", ")
            ));
        }
        out.push_str("\n## Functions of this cell (a starting list: the subject's and the obligation's code, their bases and derived contracts, the writers and readers of their state, one hop of calls, and the entry points that reach them; a function outside it is questioned once, and kept if you submit it again with `claim` saying how it reaches the obligation)\n");
        let mut last_contract = String::new();
        for function in functions.iter().filter(|f| !f.read_only) {
            if function.contract != last_contract {
                last_contract = function.contract.clone();
                out.push_str(&format!("\n### {}\n", function.contract));
            }
            out.push_str(&format!("- {function}\n"));
        }
        let read_only: Vec<&ExtentFunction> = functions.iter().filter(|f| f.read_only).collect();
        if !read_only.is_empty() {
            out.push_str("\n### Read-only\n");
            for function in read_only {
                out.push_str(&format!("- {function}\n"));
            }
        }
        out.push_str(&format!(
            "\n## Protocol snapshot\n{}\n\n## Payload shape of `submit_task`\n```json\n{{\n  \"title\": \"short title naming the function and the obligation\",\n  \"violation\": \"post-state over project state showing this obligation failing\",\n  \"contract\": \"Vault\",\n  \"function\": \"withdraw\",\n  \"claim\": \"an unprivileged external caller can reach <the violation> through Vault.withdraw\",\n  \"consumes\": [\n    {{\"input\": \"shares (the amount burned)\", \"kind\": \"parameter\", \"decided_by\": \"the caller\"}},\n    {{\"input\": \"owner (whose shares are burned)\", \"kind\": \"parameter\", \"decided_by\": \"the caller\"}},\n    {{\"input\": \"the signature over (owner, shares, nonce)\", \"kind\": \"authorization\", \"decided_by\": \"the signer\"}},\n    {{\"input\": \"receiver (who is paid)\", \"kind\": \"parameter\", \"decided_by\": \"the caller\"}},\n    {{\"input\": \"the price read from the oracle at execution\", \"kind\": \"external\", \"decided_by\": \"the callee PriceFeed\"}},\n    {{\"input\": \"the fee rate read from storage\", \"kind\": \"configuration\", \"decided_by\": \"the owner\"}}\n  ],\n  \"binds\": [\n    {{\"input\": \"shares (the amount burned)\", \"check\": \"_spendAllowance(owner, msg.sender, shares) and the burn, Vault.withdraw, src/Vault.sol:88-92\", \"fixes\": \"shares to at most the caller's own allowance from owner\"}},\n    {{\"input\": \"the signature over (owner, shares, nonce)\", \"check\": \"ecrecover over (owner, shares, nonce) == owner, Vault.withdraw, src/Vault.sol:95\", \"fixes\": \"owner, shares and nonce to the signer's signed fields\"}}\n  ]\n}}\n```\nHere `owner`, `receiver`, the oracle price and the fee rate are open: the task's reproductions move them. `contract` and `function` are copied from the listing or from the code; `binds[].input` is an entry of `consumes` copied exactly; every `binds` entry carries `check` and `fixes` (what the value is fixed to and who decided it; a non-zero or range check alone is not a fix); every `consumes` entry carries `kind` and `decided_by`.\n\nRead the function, then call `submit_task`.",
            matrix.scope_snapshot
        ));
        out
    }

    // ---- stage 4: the scenarios of one task ------------------------------------------------

    pub fn units_system(&self, max_scenarios: usize) -> String {
        let kinds = [
            ("parameter", InputKind::Parameter),
            ("value", InputKind::Value),
            ("storage", InputKind::Storage),
            ("external", InputKind::External),
            ("authorization", InputKind::Authorization),
            ("configuration", InputKind::Configuration),
            ("moment", InputKind::Moment),
            ("record", InputKind::Record),
        ]
        .iter()
        .map(|(name, kind)| format!("- {name}: {}", kind.move_template("the input", "the function")))
        .collect::<Vec<_>>()
        .join("\n");
        format!(
            "# What this work is\n\n\
             You are one stage of an authorized smart-contract security audit run by the auditing firm the code's owner engaged. The Solidity under examination is the client's own repository, supplied for review; the pipeline reads it, states where its guarantees can break, and reports back to the client under coordinated disclosure. The tests designed here compile and run locally against a fresh in-process EVM built from the repository's own source, never against a live network; they exist so a defect can be confirmed rather than argued about, and so the engineers who own the code can verify their fix.\n\n\
             # Your role\n\n\
             You design the reproduction tests of one task of the audit's coverage matrix. The task is the ledger of one function against one cell's obligation: what the function's execution consumes, and which of those inputs a check binds. Every consumed input no check binds is open. You write the scenarios that move the open inputs; a reproduction agent builds each scenario as one Foundry test compiled against this repository, and a test that reaches the cell's violating post-state becomes a finding for the client.\n\n\
             {threat}\n\n\
             ## What a scenario is\n\
             - One distinct reproduction test of the task's function: `title` (one line naming what the test reproduces), `moves` (the open inputs it moves together, each `input` copied exactly from the open inputs, with `how`: the value it takes, when, and through which call), `sequence` (the calls in order, each with `caller`, `contract` and `function` copied from the listing or from the code, and `intention`), `observe` (the post-state, over project state, that shows the cell's promise failing).\n\
             - The sequence starts with a call its caller can send. A function reached only internally or as a callback is reached through the entry point that calls it: that entry point comes first.\n\
             - Group inputs into one scenario when one test moves them together: the amount and the recipient of one call, the storage another entry point writes and the call that then reads it. Keep inputs apart when moving them are different tests.\n\
             - The scenarios of a task differ in what they move or how. Two scenarios that would build the same test are one scenario. Write as many as the distinct tests need, at most {max_scenarios}.\n\
             - Every open input is moved by at least one scenario.\n\
             ## How an open input of each kind is moved\n\
             {kinds}\n\
             ## Rules\n\
             - Stay within the adversary model: a sequence uses no privileged role beyond the configuration changes the code permits, and no cheat on project storage; setup may use a trusted role only to create a realistic state that the protocol's own deployment or normal operation would create.\n\
             - Stay on the protocol's real chain: precompiles, system contracts, oracles and standard tokens behave as they do in production; a dependency's real deviations count, its misbehaviour does not.\n\
             - Read the code: confirm every function you name exists with the signature you rely on.\n\n\
             {scope}\n\n## Project overview\n{overview}\n\n{tools}\n\n\
             Read the function and where its open inputs come from, then call `submit_units` with this task's scenarios. A rejected submission names the problems; fix them and submit again.",
            threat = self.threat_model(),
            scope = self.render_scope(),
            overview = self.overview,
            tools = self.tool_guide,
        )
    }

    pub fn units_user(
        &self,
        matrix: &CoverageMatrix,
        task: &VerificationTask,
        functions: &[ExtentFunction],
        max_scenarios: usize,
    ) -> String {
        let mut out = format!("Write the scenarios of task {}.\n\n", task.id);
        for c in &task.cells {
            if let Some(s) = matrix.subjects.get(&c.cell.subject_id) {
                out.push_str(&format!("## Subject\n{}\n\n", Self::render_subject(s)));
            }
            if let Some(p) = matrix.properties.get(&c.cell.property_id) {
                out.push_str(&format!("## Property\n{} `{}`: {}\n\n", p.id, p.name, p.statement));
            }
            if let Some(o) = matrix.obligations.iter().find(|o| o.cell == c.cell) {
                out.push_str(&format!("## Obligation\n{}\n\n", o.statement));
            }
            out.push_str(&format!("## Violation (the task author's post-state)\n{}\n\n", c.violation));
        }
        let function = task
            .function
            .as_ref()
            .map(|f| f.to_string())
            .unwrap_or_else(|| "?".to_string());
        out.push_str(&format!("## Task\nFunction: {function}\nClaim: {}\n", task.claim));
        if !task.binds.is_empty() {
            out.push_str("\nBound inputs (a check fixes them; not moved):\n");
            for b in &task.binds {
                out.push_str(&format!("- `{}`: {} → {}\n", b.input, b.check, b.fixes));
            }
        }
        out.push_str("\nOpen inputs (every one is moved by at least one scenario; copy `input` exactly):\n");
        for c in task.open_inputs() {
            out.push_str(&format!("- `{}` [{:?}, decided by {}]\n", c.input, c.kind, c.decided_by));
        }
        out.push_str("\n## Functions of this task (a starting list: the task agent's listing and the task's function; a call outside it is questioned once, and kept if you submit it again with the step's `intention` saying why the sequence needs it)\n");
        let mut last_contract = String::new();
        for function in functions.iter().filter(|f| !f.read_only) {
            if function.contract != last_contract {
                last_contract = function.contract.clone();
                out.push_str(&format!("\n### {}\n", function.contract));
            }
            out.push_str(&format!("- {function}\n"));
        }
        let read_only: Vec<&ExtentFunction> = functions.iter().filter(|f| f.read_only).collect();
        if !read_only.is_empty() {
            out.push_str("\n### Read-only\n");
            for function in read_only {
                out.push_str(&format!("- {function}\n"));
            }
        }
        out.push_str(&format!(
            "\n## Payload shape of `submit_units`\n```json\n{{\n  \"scenarios\": [\n    {{\n      \"title\": \"withdraw burns the owner's shares and pays a receiver the owner never chose\",\n      \"moves\": [\n        {{\"input\": \"owner (whose shares are burned)\", \"how\": \"an account holding shares that approved the caller for a smaller amount\"}},\n        {{\"input\": \"receiver (who is paid)\", \"how\": \"the caller's own address\"}}\n      ],\n      \"sequence\": [\n        {{\"caller\": \"owner\", \"contract\": \"Vault\", \"function\": \"deposit\", \"intention\": \"owner holds shares\"}},\n        {{\"caller\": \"unprivileged caller\", \"contract\": \"Vault\", \"function\": \"withdraw\", \"intention\": \"burn owner's shares and pay the caller\"}}\n      ],\n      \"observe\": \"the caller received assets for shares owner never authorised it to redeem\"\n    }},\n    {{\n      \"title\": \"withdraw settles at an oracle price moved within the transaction\",\n      \"moves\": [\n        {{\"input\": \"the price read from the oracle at execution\", \"how\": \"moved by a swap in the pool the oracle reads, in the same transaction\"}}\n      ],\n      \"sequence\": [\n        {{\"caller\": \"unprivileged caller\", \"contract\": \"Vault\", \"function\": \"withdraw\", \"intention\": \"redeem right after the price moved\"}}\n      ],\n      \"observe\": \"the caller received more assets than its shares are worth at the unmoved price\"\n    }}\n  ]\n}}\n```\nAt most {max_scenarios} scenarios; every open input is moved by at least one; `moves[].input` is an open input copied exactly; `sequence[].contract` and `function` are copied from the listing or from the code.\n\nRead the function, then call `submit_units`."
        ));
        out
    }

    /// Sent whenever a unit agent has spent `step` more steps without calling `submit_units`.
    pub fn units_checkpoint(step: usize) -> String {
        format!(
            "You have used {step} more steps without calling `submit_units`. Stop reading and submit this task's scenarios now: the distinct reproduction tests, the open inputs each moves, the calls from the first one its caller sends, and what each observes. A rejected submission returns the exact problems, which is cheaper than another file read."
        )
    }

    // ---- nudges ---------------------------------------------------------------------------

    /// Sent whenever a task agent has spent `step` more steps without calling `submit_task`.
    pub fn task_checkpoint(step: usize) -> String {
        format!(
            "You have used {step} more steps without calling `submit_task`. Stop reading and submit this cell's task now: the function its violation goes through and that function's ledger (consumes with kind and decider; binds with check and fixes). A rejected submission returns the exact problems, which is cheaper than another file read."
        )
    }

    pub fn nudge(tool: &str) -> String {
        format!(
            "You ended your turn without calling `{tool}`. Call it now with the complete payload. If the last submission was rejected, fix every reported problem and resubmit."
        )
    }
}

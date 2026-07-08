package ai.oneai

import android.util.Log
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.compose.animation.core.LinearEasing
import androidx.compose.animation.core.RepeatMode
import androidx.compose.animation.core.animateFloat
import androidx.compose.animation.core.infiniteRepeatable
import androidx.compose.animation.core.rememberInfiniteTransition
import androidx.compose.animation.core.tween
import androidx.compose.foundation.clickable
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.imePadding
import androidx.compose.foundation.layout.navigationBarsPadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.lazy.rememberLazyListState
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.Send
import androidx.compose.material.icons.automirrored.filled.KeyboardArrowRight
import androidx.compose.material.icons.filled.ArrowDownward
import androidx.compose.material.icons.filled.KeyboardArrowDown
import androidx.compose.material.icons.filled.Psychology
import androidx.compose.material.icons.filled.Settings
import androidx.compose.material.icons.filled.Stop
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.FloatingActionButton
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.OutlinedTextFieldDefaults
import androidx.compose.material3.Scaffold
import androidx.compose.material3.SmallFloatingActionButton
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TopAppBar
import androidx.compose.material3.TopAppBarDefaults
import androidx.compose.material3.lightColorScheme
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.derivedStateOf
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateListOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.runtime.snapshotFlow
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.AnnotatedString
import androidx.compose.ui.text.SpanStyle
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.buildAnnotatedString
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.input.KeyboardCapitalization
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.text.withStyle
import androidx.compose.ui.unit.TextUnit
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import kotlinx.coroutines.flow.distinctUntilChanged
import kotlinx.coroutines.launch
import uniffi.oneai.ChatEventCallback
import uniffi.oneai.ChatEventView
import uniffi.oneai.OneAiApp
import uniffi.oneai.OneAiAppBuilder
import uniffi.oneai.OneAiSession
import uniffi.oneai.ProviderConfigView

// ──────────────────────────────────────────────────────────────────────
// OneAI Android chat — S3 (豆包-style rewrite).
//
// Single Compose screen wired through the FFI inference loop:
//   input box → session.runTask(task, callback)
//   ChatEventCallback (fires on the tokio worker thread; every state
//   mutation marshalled to main via runOnUiThread):
//     StreamChunk   → append to live answer (typewriter + blinking cursor)
//     Thinking      → accumulate into ONE collapsible "思考过程" card
//     ToolCall/Result → compact dim step lines
//     DirectAnswer/Complete → finalize, stop cursor
//     Error         → inline error
//
// Scrolling: a sentinel item at the end + a derivedStateOf `atBottom`
// (from canScrollForward). While at bottom, stream chunks auto-stick to
// the bottom; once the user scrolls up, auto-scroll stops and a
// "回到底部" FAB appears. Tapping stop calls session.interrupt().
//
// Markdown: lightweight self-render — fenced ```code blocks``` (monospace
// card) + inline `code` + **bold** + bullet/list prefix; no extra deps.
// ──────────────────────────────────────────────────────────────────────

private const val TAG = "OneAI"

// ── 豆包-ish light palette ───────────────────────────────────────────
private val BgChat = Color(0xFFF7F7F8)
private val SurfWhite = Color(0xFFFFFFFF)
private val TextPrimary = Color(0xFF1A1A1A)
private val TextDim = Color(0xFF8A8A8A)
private val UserBubble = Color(0xFFE7F0FF)
private val CodeBg = Color(0xFFF2F3F5)
private val Accent = Color(0xFF4D6BFE)

private val DoubaoColors = lightColorScheme(
    background = BgChat,
    surface = SurfWhite,
    onBackground = TextPrimary,
    onSurface = TextPrimary,
    onSurfaceVariant = TextDim,
    primary = Accent,
    onPrimary = Color.White,
    surfaceVariant = Color(0xFFEFEFF1),
)

class MainActivity : ComponentActivity() {
    override fun onCreate(savedInstanceState: android.os.Bundle?) {
        super.onCreate(savedInstanceState)
        enableEdgeToEdge()
        setContent {
            MaterialTheme(colorScheme = DoubaoColors) {
                Surface(
                    modifier = Modifier.fillMaxSize(),
                    color = BgChat,
                ) {
                    ChatScreen(activity = this)
                }
            }
        }
    }
}

// ── Chat model ───────────────────────────────────────────────────────
// LazyColumn keys must be Bundle-saveable on Android → Long, never Any().

private sealed interface ChatItem {
    val key: Long
}

private data class UserItem(val text: String, override val key: Long) : ChatItem

private class AssistantItem(override val key: Long) : ChatItem {
    /** Accumulated reasoning text — one block, not one bubble per chunk. */
    var thinking by mutableStateOf("")
    var thinkingActive by mutableStateOf(false)
    var thinkingDone by mutableStateOf(false)
    var thinkingExpanded by mutableStateOf(false)

    /** Compact tool trace (one line each — fine, these are infrequent). */
    val steps = mutableStateListOf<ToolStep>()

    /** Streamed answer. */
    var text by mutableStateOf("")
    var streaming by mutableStateOf(false)
    var done by mutableStateOf(false)
    var error by mutableStateOf<String?>(null)
}

private data class ToolStep(
    val callId: String,
    val name: String,
    val args: String,
    var result: String? = null,
    var ok: Boolean? = null,
)

// ── Screen ───────────────────────────────────────────────────────────

@OptIn(ExperimentalMaterial3Api::class)
@Composable
private fun ChatScreen(activity: ComponentActivity) {
    val vm = remember { ChatViewModel(activity) }
    val scope = rememberCoroutineScope()
    val listState = rememberLazyListState()

    // atBottom: true when the list can't scroll further down (or is empty).
    // Reactive via derivedStateOf so the FAB + auto-scroll see live state.
    val atBottom by remember {
        derivedStateOf {
            listState.layoutInfo.totalItemsCount == 0 || !listState.canScrollForward
        }
    }

    Scaffold(
        containerColor = BgChat,
        topBar = {
            TopAppBar(
                title = { Text("OneAI", color = TextPrimary) },
                actions = {
                    IconButton(onClick = { vm.showConfig = !vm.showConfig }) {
                        Icon(Icons.Filled.Settings, contentDescription = "Provider settings", tint = TextDim)
                    }
                },
                colors = TopAppBarDefaults.topAppBarColors(containerColor = BgChat),
            )
        },
        bottomBar = {
            InputBar(
                value = vm.input,
                running = vm.running,
                onChange = { vm.input = it },
                onSend = {
                    val task = vm.input.trim()
                    if (task.isNotEmpty() && !vm.running) {
                        vm.input = ""
                        scope.launch { vm.runTask(task) }
                    }
                },
                onStop = { scope.launch { vm.stop() } },
            )
        },
    ) { inner ->
        Box(modifier = Modifier.padding(inner).fillMaxSize()) {
            Column(modifier = Modifier.fillMaxSize()) {
                if (vm.showConfig) ProviderConfigCard(vm)

                LazyColumn(
                    state = listState,
                    modifier = Modifier.weight(1f).fillMaxWidth(),
                    verticalArrangement = Arrangement.spacedBy(18.dp),
                    contentPadding = androidx.compose.foundation.layout.PaddingValues(
                        start = 12.dp, end = 12.dp, top = 12.dp, bottom = 12.dp,
                    ),
                ) {
                    items(vm.items, key = { it.key }) { item ->
                        when (item) {
                            is UserItem -> UserBubble(item.text)
                            is AssistantItem -> AssistantBubble(item)
                        }
                    }
                    // Sentinel: scrolling to items.size (this item's index)
                    // clamps to max-scroll = stick to bottom. Also reserves a
                    // little breathing room under the last message.
                    item(key = "sentinel") { Spacer(Modifier.height(1.dp)) }
                }
                vm.error?.let { msg ->
                    Text(
                        "✗ $msg",
                        color = MaterialTheme.colorScheme.error,
                        modifier = Modifier.padding(horizontal = 12.dp, vertical = 4.dp),
                        fontSize = 13.sp,
                    )
                }
            }

            // 回到底部 — only when the user has scrolled up during/after a turn.
            if (!atBottom && vm.items.isNotEmpty()) {
                androidx.compose.foundation.layout.Box(
                    modifier = Modifier.align(Alignment.BottomEnd).padding(end = 16.dp, bottom = 16.dp),
                ) {
                    SmallFloatingActionButton(
                        onClick = {
                            scope.launch { listState.animateScrollToItem(vm.items.size) }
                        },
                        containerColor = SurfWhite,
                        contentColor = Accent,
                    ) {
                        Icon(Icons.Filled.ArrowDownward, contentDescription = "回到底部")
                    }
                }
            }
        }
    }

    // Auto-stick-to-bottom while streaming, but only if the user hasn't
    // scrolled away. Keyed on streamTick (fires per content chunk) + size.
    LaunchedEffect(vm.items.size, vm.streamTick) {
        if (vm.items.isNotEmpty() && atBottom) {
            listState.animateScrollToItem(vm.items.size)
        }
    }

    // If the user scrolls back to the bottom during a turn, resume following.
    LaunchedEffect(vm.running) {
        snapshotFlow { atBottom }
            .distinctUntilChanged()
            .collect { bottom ->
                if (bottom && vm.items.isNotEmpty()) {
                    listState.animateScrollToItem(vm.items.size)
                }
            }
    }
}

// ── Assistant bubble: thinking card + steps + answer + cursor ───────

@Composable
private fun AssistantBubble(item: AssistantItem) {
    Column(modifier = Modifier.fillMaxWidth()) {
        ThinkingCard(item)
        if (item.steps.isNotEmpty()) {
            Column(modifier = Modifier.padding(top = 6.dp, bottom = 4.dp)) {
                item.steps.forEach { StepLine(it) }
            }
        }
        if (item.text.isNotEmpty()) {
            MarkdownText(
                text = item.text,
                color = TextPrimary,
                fontSize = 15.sp,
                modifier = Modifier.fillMaxWidth(),
            )
        }
        if (item.streaming && item.text.isNotEmpty()) {
            BlinkingCursor(modifier = Modifier.padding(top = 2.dp, start = 2.dp))
        }
        item.error?.let { msg ->
            Text(
                "✗ $msg",
                color = MaterialTheme.colorScheme.error,
                fontSize = 13.sp,
                modifier = Modifier.padding(top = 4.dp),
            )
        }
    }
}

@Composable
private fun ThinkingCard(item: AssistantItem) {
    if (item.thinking.isEmpty()) return
    val expanded = item.thinkingActive || item.thinkingExpanded
    Surface(
        color = Color(0xFFF2F4FB),
        shape = RoundedCornerShape(10.dp),
        modifier = Modifier.fillMaxWidth(),
    ) {
        Column(modifier = Modifier.padding(horizontal = 10.dp, vertical = 8.dp)) {
            Row(
                verticalAlignment = Alignment.CenterVertically,
                modifier = Modifier
                    .fillMaxWidth()
                    .let { if (item.thinkingDone) it.then(Modifier.clickable { item.thinkingExpanded = !item.thinkingExpanded }) else it },
            ) {
                Icon(
                    Icons.Filled.Psychology,
                    contentDescription = null,
                    tint = Accent,
                    modifier = Modifier.size(15.dp),
                )
                Spacer(Modifier.width(6.dp))
                Text(
                    if (item.thinkingActive) "思考中…" else "已深度思考",
                    color = TextDim,
                    fontSize = 12.sp,
                )
                if (item.thinkingActive) {
                    Spacer(Modifier.width(6.dp))
                    ThreeDots()
                } else {
                    Spacer(Modifier.weight(1f))
                    Icon(
                        if (expanded) Icons.Filled.KeyboardArrowDown else Icons.AutoMirrored.Filled.KeyboardArrowRight,
                        contentDescription = if (expanded) "收起" else "展开",
                        tint = TextDim,
                        modifier = Modifier.size(16.dp),
                    )
                }
            }
            if (expanded) {
                Box(
                    modifier = Modifier
                        .padding(top = 6.dp)
                        .heightIn(max = 260.dp)
                        .verticalScroll(rememberScrollState()),
                ) {
                    Text(
                        item.thinking,
                        color = TextDim,
                        fontSize = 13.sp,
                    )
                }
            }
        }
    }
}

@Composable
private fun StepLine(step: ToolStep) {
    val (icon, tint) = when (step.ok) {
        true -> "✓" to Color(0xFF3B8C5A)
        false -> "✗" to MaterialTheme.colorScheme.error
        null -> "⚙" to TextDim
    }
    Column(modifier = Modifier.padding(vertical = 2.dp)) {
        Text(
            "$icon ${step.name}(${step.args})",
            color = tint,
            fontSize = 11.sp,
            fontFamily = FontFamily.Monospace,
        )
        step.result?.let { r ->
            Text(
                "    └ ${r.take(200)}",
                color = TextDim,
                fontSize = 11.sp,
                fontFamily = FontFamily.Monospace,
            )
        }
    }
}

// ── Bubbles ──────────────────────────────────────────────────────────

@Composable
private fun UserBubble(text: String) {
    Column(modifier = Modifier.fillMaxWidth(), horizontalAlignment = Alignment.End) {
        Surface(
            color = UserBubble,
            shape = RoundedCornerShape(16.dp),
            modifier = Modifier.widthIn(max = 320.dp),
        ) {
            Text(
                text,
                color = TextPrimary,
                modifier = Modifier.padding(horizontal = 12.dp, vertical = 8.dp),
                fontSize = 15.sp,
            )
        }
    }
}

// ── Markdown (lightweight, no deps) ──────────────────────────────────

private sealed interface MdSeg {
    data class Prose(val text: String) : MdSeg
    data class Code(val lang: String, val code: String) : MdSeg
}

@Composable
private fun MarkdownText(
    text: String,
    color: Color,
    fontSize: TextUnit,
    modifier: Modifier = Modifier,
) {
    val segs = remember(text) { splitMarkdown(text) }
    Column(modifier = modifier, verticalArrangement = Arrangement.spacedBy(6.dp)) {
        segs.forEach { seg ->
            when (seg) {
                is MdSeg.Code -> CodeCard(seg.lang, seg.code)
                is MdSeg.Prose -> Text(
                    text = buildInline(seg.text),
                    color = color,
                    fontSize = fontSize,
                )
            }
        }
    }
}

@Composable
private fun CodeCard(lang: String, code: String) {
    Surface(
        color = CodeBg,
        shape = RoundedCornerShape(8.dp),
        modifier = Modifier.fillMaxWidth(),
    ) {
        Column(modifier = Modifier.padding(10.dp)) {
            if (lang.isNotEmpty()) {
                Text(
                    lang,
                    color = TextDim,
                    fontSize = 11.sp,
                    fontFamily = FontFamily.Monospace,
                    modifier = Modifier.padding(bottom = 4.dp),
                )
            }
            Text(
                code,
                color = TextPrimary,
                fontSize = 13.sp,
                fontFamily = FontFamily.Monospace,
                modifier = Modifier
                    .fillMaxWidth()
                    .horizontalScroll(rememberScrollState()),
            )
        }
    }
}

private fun splitMarkdown(src: String): List<MdSeg> {
    val out = mutableListOf<MdSeg>()
    val lines = src.split("\n")
    val buf = StringBuilder()
    fun flush() {
        if (buf.isNotEmpty()) {
            out.add(MdSeg.Prose(buf.toString().trimEnd('\n')))
            buf.clear()
        }
    }
    var i = 0
    while (i < lines.size) {
        val l = lines[i]
        if (l.trimStart().startsWith("```")) {
            flush()
            val lang = l.trimStart().removePrefix("```").trim()
            val code = StringBuilder()
            i++
            while (i < lines.size && !lines[i].trimStart().startsWith("```")) {
                code.append(lines[i]).append('\n')
                i++
            }
            i++ // skip closing fence (if any)
            out.add(MdSeg.Code(lang, code.toString().trimEnd('\n')))
        } else {
            buf.append(l).append('\n')
            i++
        }
    }
    flush()
    return out
}

/** Render inline `code` and **bold**, plus bullet/numbered list prefixes. */
private fun buildInline(src: String): AnnotatedString = buildAnnotatedString {
    for (rawLine in src.split("\n")) {
        val trimmed = rawLine.trimEnd()
        if (trimmed.isEmpty()) {
            append('\n')
            continue
        }
        // List markers → bullet prefix (numbered stays as-is text).
        val (prefix, body) = when {
            trimmed.startsWith("- ") || trimmed.startsWith("* ") -> "•  " to trimmed.substring(2)
            else -> "" to trimmed
        }
        if (prefix.isNotEmpty()) append(prefix)
        appendInline(body, this)
        append('\n')
    }
}

private fun appendInline(s: String, b: AnnotatedString.Builder) {
    var i = 0
    while (i < s.length) {
        when {
            s.startsWith("**", i) -> {
                val end = s.indexOf("**", i + 2)
                if (end >= 0) {
                    b.withStyle(SpanStyle(fontWeight = FontWeight.Bold)) {
                        appendInline(s.substring(i + 2, end), this)
                    }
                    i = end + 2
                } else {
                    b.append(s[i]); i++
                }
            }
            s[i] == '`' -> {
                val end = s.indexOf('`', i + 1)
                if (end >= 0) {
                    b.withStyle(SpanStyle(fontFamily = FontFamily.Monospace, background = CodeBg)) {
                        append(s.substring(i + 1, end))
                    }
                    i = end + 1
                } else {
                    b.append(s[i]); i++
                }
            }
            else -> {
                b.append(s[i]); i++
            }
        }
    }
}

// ── Streaming cursor + thinking indicator ───────────────────────────

@Composable
private fun BlinkingCursor(modifier: Modifier = Modifier) {
    val t = rememberInfiniteTransition(label = "cursor")
    val alpha by t.animateFloat(
        initialValue = 1f,
        targetValue = 0.2f,
        animationSpec = infiniteRepeatable(
            animation = androidx.compose.animation.core.tween(500, easing = LinearEasing),
            repeatMode = RepeatMode.Reverse,
        ),
        label = "cursorAlpha",
    )
    Text("▍", color = Accent.copy(alpha = alpha), fontSize = 15.sp, modifier = modifier)
}

@Composable
private fun ThreeDots() {
    val t = rememberInfiniteTransition(label = "dots")
    Row {
        repeat(3) { idx ->
            val a by t.animateFloat(
                initialValue = 0.3f,
                targetValue = 1f,
                animationSpec = infiniteRepeatable(
                    animation = androidx.compose.animation.core.tween(600, delayMillis = idx * 150, easing = LinearEasing),
                    repeatMode = RepeatMode.Reverse,
                ),
                label = "dot$idx",
            )
            Text("·", color = TextDim.copy(alpha = a), fontSize = 16.sp)
        }
    }
}

// ── Input bar (pill + send/stop) ────────────────────────────────────

@OptIn(ExperimentalMaterial3Api::class)
@Composable
private fun InputBar(
    value: String,
    running: Boolean,
    onChange: (String) -> Unit,
    onSend: () -> Unit,
    onStop: () -> Unit,
) {
    Surface(color = SurfWhite, tonalElevation = 2.dp, modifier = Modifier.fillMaxWidth()) {
        Row(
            modifier = Modifier
                .fillMaxWidth()
                .navigationBarsPadding()
                .imePadding()
                .padding(8.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            OutlinedTextField(
                value = value,
                onValueChange = onChange,
                modifier = Modifier.weight(1f).heightIn(min = 48.dp),
                placeholder = { Text("问点什么…", color = TextDim) },
                maxLines = 4,
                textStyle = TextStyle(fontSize = 15.sp, color = TextPrimary),
                shape = RoundedCornerShape(20.dp),
                colors = OutlinedTextFieldDefaults.colors(
                    focusedContainerColor = CodeBg,
                    unfocusedContainerColor = CodeBg,
                    focusedBorderColor = Color.Transparent,
                    unfocusedBorderColor = Color.Transparent,
                ),
                keyboardOptions = KeyboardOptions(capitalization = KeyboardCapitalization.Sentences),
            )
            Spacer(Modifier.width(8.dp))
            if (running) {
                FloatingActionButton(
                    onClick = onStop,
                    shape = CircleShape,
                    containerColor = Color(0xFFE5484D),
                    contentColor = Color.White,
                    modifier = Modifier.size(46.dp),
                ) {
                    Icon(Icons.Filled.Stop, contentDescription = "停止", modifier = Modifier.size(22.dp))
                }
            } else {
                FloatingActionButton(
                    onClick = onSend,
                    shape = CircleShape,
                    containerColor = if (value.isNotBlank()) Accent else Color(0xFFD8D8D8),
                    contentColor = Color.White,
                    modifier = Modifier.size(46.dp),
                ) {
                    Icon(Icons.AutoMirrored.Filled.Send, contentDescription = "发送", modifier = Modifier.size(20.dp))
                }
            }
        }
    }
}

// ── Provider config card ─────────────────────────────────────────────

@Composable
private fun ProviderConfigCard(vm: ChatViewModel) {
    Column(
        modifier = Modifier
            .fillMaxWidth()
            .padding(horizontal = 12.dp, vertical = 4.dp),
        verticalArrangement = Arrangement.spacedBy(6.dp),
    ) {
        Text("Provider", style = TextStyle(fontSize = 13.sp, fontWeight = FontWeight.SemiBold), color = TextDim)
        Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            OutlinedTextField(
                value = vm.kind,
                onValueChange = { vm.kind = it },
                label = { Text("kind") },
                singleLine = true,
                modifier = Modifier.weight(0.4f),
                textStyle = TextStyle(fontSize = 13.sp, fontFamily = FontFamily.Monospace),
            )
            OutlinedTextField(
                value = vm.model,
                onValueChange = { vm.model = it },
                label = { Text("model") },
                singleLine = true,
                modifier = Modifier.weight(0.6f),
                textStyle = TextStyle(fontSize = 13.sp, fontFamily = FontFamily.Monospace),
            )
        }
        OutlinedTextField(
            value = vm.apiKey,
            onValueChange = { vm.apiKey = it },
            label = { Text("api key (openai / anthropic)") },
            singleLine = true,
            modifier = Modifier.fillMaxWidth(),
            textStyle = TextStyle(fontSize = 13.sp, fontFamily = FontFamily.Monospace),
            keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Password),
        )
        OutlinedTextField(
            value = vm.baseUrl,
            onValueChange = { vm.baseUrl = it },
            label = { Text("base url override (blank = provider default)") },
            singleLine = true,
            modifier = Modifier.fillMaxWidth(),
            textStyle = TextStyle(fontSize = 13.sp, fontFamily = FontFamily.Monospace),
        )
        Text(
            "tip: ollama on the host emulator → kind=ollama, model=llama3, base url=http://10.0.2.2:11434",
            fontSize = 11.sp,
            color = TextDim,
        )
    }
}

// ── ViewModel (plain holder — no androidx.lifecycle dep) ─────────────

private class ChatViewModel(private val activity: ComponentActivity) {
    var kind by mutableStateOf("openai")
    var model by mutableStateOf("gpt-4o-mini")
    var apiKey by mutableStateOf("")
    var baseUrl by mutableStateOf("")
    var showConfig by mutableStateOf(false)

    val items = mutableStateListOf<ChatItem>()
    var input by mutableStateOf("")
    var running by mutableStateOf(false)
    var error by mutableStateOf<String?>(null)

    /** Bumped on every content-bearing event so the auto-scroll LaunchedEffect
     *  re-fires during streaming (items.size alone wouldn't). */
    var streamTick by mutableStateOf(0L)
        private set

    private var app: OneAiApp? = null
    private var session: OneAiSession? = null
    private var keySeq = 0L

    private fun nextKey(): Long { keySeq += 1; return keySeq }
    private fun tick() { streamTick += 1 }

    suspend fun runTask(task: String) {
        val s = session ?: ensureSession()
        if (s == null) {
            error = "session not built"
            return
        }
        items.add(UserItem(text = task, key = nextKey()))
        val turn = AssistantItem(key = nextKey())
        items.add(turn)
        running = true
        error = null

        val callback = object : ChatEventCallback {
            override fun onEvent(event: ChatEventView) {
                activity.runOnUiThread {
                    when (event) {
                        is ChatEventView.Thinking -> {
                            turn.thinkingActive = true
                            turn.thinking += event.text
                            tick()
                        }
                        is ChatEventView.StreamChunk -> {
                            // First answer chunk finalizes the thinking card.
                            if (turn.thinkingActive) {
                                turn.thinkingActive = false
                                turn.thinkingDone = true
                            }
                            turn.streaming = true
                            turn.text += event.text
                            tick()
                        }
                        is ChatEventView.ToolCall -> {
                            turn.steps.add(ToolStep(event.id, event.name, event.argsJson))
                            tick()
                        }
                        is ChatEventView.ToolResult -> {
                            val step = turn.steps.firstOrNull { it.callId == event.callId }
                                ?: turn.steps.lastOrNull { it.result == null }
                            if (step != null) {
                                step.result = event.content
                                step.ok = event.success
                            }
                            tick()
                        }
                        is ChatEventView.DirectAnswer -> {
                            if (event.text.isNotBlank()) turn.text = event.text
                            if (turn.thinkingActive) { turn.thinkingActive = false; turn.thinkingDone = true }
                            tick()
                        }
                        is ChatEventView.Complete -> {
                            if (event.finalText.isNotBlank()) turn.text = event.finalText
                            if (turn.thinkingActive) { turn.thinkingActive = false; turn.thinkingDone = true }
                            turn.streaming = false
                            turn.done = true
                            running = false
                            tick()
                        }
                        is ChatEventView.Error -> {
                            turn.error = event.message
                            turn.streaming = false
                            turn.done = true
                            running = false
                            tick()
                        }
                    }
                }
            }
        }

        try {
            s.runTask(task, callback)
            activity.runOnUiThread {
                turn.streaming = false
                turn.done = true
                running = false
                tick()
            }
        } catch (e: Throwable) {
            Log.e(TAG, "runTask failed", e)
            activity.runOnUiThread {
                turn.error = e.message ?: e::class.simpleName
                turn.streaming = false
                turn.done = true
                running = false
                tick()
            }
        }
    }

    suspend fun stop() {
        try {
            session?.interrupt()
        } catch (e: Throwable) {
            Log.e(TAG, "interrupt failed", e)
        }
    }

    private suspend fun ensureSession(): OneAiSession? {
        return try {
            val cfg = ProviderConfigView(
                kind = kind.trim().ifEmpty { "openai" },
                apiKey = apiKey.trim().ifBlank { null },
                baseUrl = baseUrl.trim().ifBlank { null },
                model = model.trim().ifEmpty { "gpt-4o-mini" },
                host = null,
                port = null,
            )
            // provider_config() consumes the builder Arc (Rust: self: Arc<Self>
            // → take_inner()) and returns a NEW Arc<Self>. The returned builder
            // MUST be used for the next call — the original handle is empty.
            val builder = OneAiAppBuilder().providerConfig(cfg)
            val a = builder.build()
            app = a
            val sess = a.createSession()
            session = sess
            sess
        } catch (e: Throwable) {
            Log.e(TAG, "ensureSession failed", e)
            activity.runOnUiThread { error = "build failed: ${e.message}" }
            null
        }
    }
}

package ai.oneai

import android.util.Log
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.imePadding
import androidx.compose.foundation.layout.navigationBarsPadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.lazy.rememberLazyListState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.Send
import androidx.compose.material.icons.filled.Settings
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.FilledIconButton
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextField
import androidx.compose.material3.TextFieldDefaults
import androidx.compose.material3.TopAppBar
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateListOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.layout.layout
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import kotlinx.coroutines.launch
import uniffi.oneai.ChatEventCallback
import uniffi.oneai.ChatEventView
import uniffi.oneai.OneAiApp
import uniffi.oneai.OneAiAppBuilder
import uniffi.oneai.OneAiSession
import uniffi.oneai.ProviderConfigView

// ──────────────────────────────────────────────────────────────────────
// OneAI Android chat — S3.
//
// A single Jetpack Compose screen that wires the full FFI inference loop:
//   input box  →  session.runTask(task, callback)
//   ChatEventCallback receives StreamChunk (typewriter append),
//   Thinking/ToolCall/ToolResult (dimmed trace), DirectAnswer/Complete
//   (finish), Error (surface message).
//
// The callback fires on the tokio worker thread; every state mutation is
// marshalled back to the main thread via runOnUiThread before touching
// Compose snapshot state. runTask is suspend and driven from a
// rememberCoroutineScope (main dispatcher).
//
// The provider is configured from on-screen fields (kind/model/apiKey/
// baseUrl) so no secrets are baked into the APK. Session is built lazily
// on first send and reused thereafter.
// ──────────────────────────────────────────────────────────────────────

private const val TAG = "OneAI"

class MainActivity : ComponentActivity() {
    override fun onCreate(savedInstanceState: android.os.Bundle?) {
        super.onCreate(savedInstanceState)
        setContent {
            OneAiTheme {
                Surface(
                    modifier = Modifier.fillMaxSize(),
                    color = MaterialTheme.colorScheme.background,
                ) {
                    ChatScreen(activity = this)
                }
            }
        }
    }
}

// ── Compose theme — lean on the default Material3 color scheme so the
//    screen is legible without a custom palette. ──────────────────────
@Composable
private fun OneAiTheme(content: @Composable () -> Unit) {
    MaterialTheme(content = content)
}

// ── Chat model ───────────────────────────────────────────────────────

private sealed interface ChatItem {
    val key: Any
}

private data class UserItem(val text: String, override val key: Any = Any()) : ChatItem

/** A live assistant turn. `text` is observable so StreamChunk appends
 *  re-render the bubble in place (typewriter). `trace` holds the
 *  dimmed thinking/tool lines. */
private class AssistantItem(override val key: Any = Any()) : ChatItem {
    var text by mutableStateOf("")
    val trace = mutableStateListOf<String>()
    var done by mutableStateOf(false)
}

// ── Screen ───────────────────────────────────────────────────────────

@OptIn(ExperimentalMaterial3Api::class)
@Composable
private fun ChatScreen(activity: ComponentActivity) {
    val vm = remember { ChatViewModel(activity) }
    val scope = rememberCoroutineScope()
    val listState = rememberLazyListState()

    Scaffold(
        topBar = {
            TopAppBar(
                title = { Text("OneAI") },
                actions = {
                    IconButton(onClick = { vm.showConfig = !vm.showConfig }) {
                        Icon(Icons.Filled.Settings, contentDescription = "Provider settings")
                    }
                },
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
            )
        },
    ) { inner ->
        Column(modifier = Modifier.padding(inner).fillMaxSize()) {
            if (vm.showConfig) ProviderConfigCard(vm)

            // Auto-scroll to the newest item as the typewriter appends.
            LaunchedEffectScroller(vm.items, listState)

            LazyColumn(
                state = listState,
                modifier = Modifier.weight(1f).fillMaxWidth(),
                verticalArrangement = Arrangement.spacedBy(8.dp),
                contentPadding = androidx.compose.foundation.layout.PaddingValues(12.dp),
            ) {
                items(vm.items, key = { it.key }) { item ->
                    when (item) {
                        is UserItem -> Bubble(item.text, mine = true)
                        is AssistantItem -> AssistantBubble(item)
                    }
                }
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
    }
}

// ── Assistant bubble: streamed text + collapsed trace ───────────────

@Composable
private fun AssistantBubble(item: AssistantItem) {
    Column(modifier = Modifier.fillMaxWidth()) {
        if (item.trace.isNotEmpty()) {
            item.trace.forEach { line ->
                Text(
                    line,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                    fontSize = 11.sp,
                    fontFamily = FontFamily.Monospace,
                    modifier = Modifier.padding(start = 4.dp, bottom = 2.dp),
                )
            }
        }
        if (item.text.isNotEmpty()) {
            Bubble(item.text, mine = false)
        }
        if (!item.done && item.text.isNotEmpty()) {
            Row(
                verticalAlignment = Alignment.CenterVertically,
                modifier = Modifier.padding(start = 4.dp, top = 2.dp),
            ) {
                CircularProgressIndicator(
                    modifier = Modifier.size(12.dp),
                    strokeWidth = 1.5.dp,
                )
                Spacer(Modifier.width(6.dp))
                Text("…", fontSize = 11.sp, color = MaterialTheme.colorScheme.onSurfaceVariant)
            }
        }
    }
}

@Composable
private fun Bubble(text: String, mine: Boolean) {
    val align = if (mine) Alignment.End else Alignment.Start
    val bg = if (mine) MaterialTheme.colorScheme.primary else MaterialTheme.colorScheme.surfaceVariant
    val fg = if (mine) MaterialTheme.colorScheme.onPrimary else MaterialTheme.colorScheme.onSurfaceVariant
    Column(
        modifier = Modifier.fillMaxWidth(),
        horizontalAlignment = align,
    ) {
        Surface(
            color = bg,
            shape = RoundedCornerShape(14.dp),
            modifier = Modifier.widthInMax(0.85f),
        ) {
            Text(
                text,
                color = fg,
                modifier = Modifier.padding(horizontal = 12.dp, vertical = 8.dp),
                fontSize = 15.sp,
            )
        }
    }
}

// Modifier.widthInMax — fraction-of-screen max width for bubbles.
private fun Modifier.widthInMax(fraction: Float) =
    this.then(layout { measurable, constraints ->
        val maxW = (constraints.maxWidth * fraction).toInt().coerceAtLeast(1)
        val placeable = measurable.measure(constraints.copy(maxWidth = maxW))
        layout(placeable.width, placeable.height) { placeable.place(0, 0) }
    })

// ── Input bar ────────────────────────────────────────────────────────

@Composable
private fun InputBar(
    value: String,
    running: Boolean,
    onChange: (String) -> Unit,
    onSend: () -> Unit,
) {
    Surface(tonalElevation = 3.dp) {
        Row(
            modifier = Modifier
                .fillMaxWidth()
                .navigationBarsPadding()
                .imePadding()
                .padding(8.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            TextField(
                value = value,
                onValueChange = onChange,
                modifier = Modifier.weight(1f).heightIn(min = 48.dp),
                placeholder = { Text("Ask OneAI…") },
                maxLines = 4,
                textStyle = TextStyle(fontSize = 15.sp),
                colors = TextFieldDefaults.colors(
                    focusedIndicatorColor = Color.Transparent,
                    unfocusedIndicatorColor = Color.Transparent,
                ),
                keyboardOptions = KeyboardOptions(capitalization = androidx.compose.ui.text.input.KeyboardCapitalization.Sentences),
            )
            Spacer(Modifier.width(8.dp))
            FilledIconButton(
                onClick = onSend,
                enabled = value.isNotBlank() && !running,
                modifier = Modifier.size(44.dp),
            ) {
                if (running) {
                    CircularProgressIndicator(
                        modifier = Modifier.size(20.dp),
                        strokeWidth = 2.dp,
                        color = MaterialTheme.colorScheme.onPrimary,
                    )
                } else {
                    Icon(Icons.AutoMirrored.Filled.Send, contentDescription = "Send")
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
            .padding(12.dp),
        verticalArrangement = Arrangement.spacedBy(6.dp),
    ) {
        Text("Provider", style = MaterialTheme.typography.labelLarge)
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
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
        HorizontalDivider()
    }
}

// ── Auto-scroll helper ───────────────────────────────────────────────

@Composable
private fun LaunchedEffectScroller(
    items: List<ChatItem>,
    state: androidx.compose.foundation.lazy.LazyListState,
) {
    androidx.compose.runtime.LaunchedEffect(items.size, items.lastOrNull()) {
        if (items.isNotEmpty()) state.animateScrollToItem(items.lastIndex)
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

    private var app: OneAiApp? = null
    private var session: OneAiSession? = null

    suspend fun runTask(task: String) {
        val s = session ?: ensureSession()
        if (s == null) {
            error = "session not built"
            return
        }
        items.add(UserItem(text = task))
        val turn = AssistantItem()
        items.add(turn)
        running = true
        error = null

        // ChatEventCallback fires on the tokio worker thread — marshal every
        // state mutation to the main thread before touching Compose state.
        val callback = object : ChatEventCallback {
            override fun onEvent(event: ChatEventView) {
                activity.runOnUiThread {
                    when (event) {
                        is ChatEventView.StreamChunk -> turn.text += event.text
                        is ChatEventView.Thinking -> turn.trace.add("💭 ${event.text}")
                        is ChatEventView.ToolCall -> turn.trace.add("⚙ ${event.name}(${event.argsJson})")
                        is ChatEventView.ToolResult -> turn.trace.add(
                            (if (event.success) "✓ " else "✗ ") + "${event.toolName}: ${event.content}",
                        )
                        is ChatEventView.DirectAnswer -> {
                            if (event.text.isNotBlank()) turn.text = event.text
                        }
                        is ChatEventView.Complete -> {
                            if (event.finalText.isNotBlank()) turn.text = event.finalText
                            turn.done = true
                            running = false
                        }
                        is ChatEventView.Error -> {
                            error = event.message
                            turn.done = true
                            running = false
                        }
                    }
                }
            }
        }

        try {
            s.runTask(task, callback)
            // runTask returned — loop ended. Complete normally fires first,
            // but guard in case the provider returned without one.
            activity.runOnUiThread {
                turn.done = true
                running = false
            }
        } catch (e: Throwable) {
            Log.e(TAG, "runTask failed", e)
            activity.runOnUiThread {
                error = e.message ?: e::class.simpleName
                turn.done = true
                running = false
            }
        }
    }

    private suspend fun ensureSession(): OneAiSession? {
        return try {
            val builder = OneAiAppBuilder()
            val cfg = ProviderConfigView(
                kind = kind.trim().ifEmpty { "openai" },
                apiKey = apiKey.trim().ifBlank { null },
                baseUrl = baseUrl.trim().ifBlank { null },
                model = model.trim().ifEmpty { "gpt-4o-mini" },
                host = null,
                port = null,
            )
            builder.providerConfig(cfg)
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

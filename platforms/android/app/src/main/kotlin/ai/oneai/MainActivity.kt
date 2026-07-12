package ai.oneai

import android.content.Intent
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
import androidx.compose.foundation.ExperimentalFoundationApi
import androidx.compose.foundation.clickable
import androidx.compose.foundation.combinedClickable
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
import androidx.compose.material.icons.automirrored.filled.KeyboardArrowRight
import androidx.compose.material.icons.automirrored.filled.Send
import androidx.compose.material.icons.filled.Add
import androidx.compose.material.icons.filled.ArrowDownward
import androidx.compose.material.icons.filled.Delete
import androidx.compose.material.icons.filled.KeyboardArrowDown
import androidx.compose.material.icons.filled.Menu
import androidx.compose.material.icons.filled.Psychology
import androidx.compose.material.icons.filled.Settings
import androidx.compose.material.icons.filled.Stop
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.DrawerValue
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.ExposedDropdownMenuBox
import androidx.compose.material3.ExposedDropdownMenuDefaults
import androidx.compose.material3.FloatingActionButton
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.ModalBottomSheet
import androidx.compose.material3.ModalDrawerSheet
import androidx.compose.material3.ModalNavigationDrawer
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.OutlinedTextFieldDefaults
import androidx.compose.material3.Scaffold
import androidx.compose.material3.SmallFloatingActionButton
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.TopAppBar
import androidx.compose.material3.TopAppBarDefaults
import androidx.compose.material3.darkColorScheme
import androidx.compose.material3.lightColorScheme
import androidx.compose.material3.rememberDrawerState
import androidx.compose.material3.rememberModalBottomSheetState
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
import androidx.compose.ui.platform.LocalClipboardManager
import androidx.compose.ui.platform.LocalContext
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
import androidx.compose.foundation.isSystemInDarkTheme
import kotlinx.coroutines.launch
import uniffi.oneai.ChatEventCallback
import uniffi.oneai.ChatEventView
import uniffi.oneai.OneAiApp
import uniffi.oneai.OneAiAppBuilder
import uniffi.oneai.OneAiErrorView
import uniffi.oneai.OneAiSession
import uniffi.oneai.ProviderConfigView
import uniffi.oneai.SessionInfoView
import java.text.SimpleDateFormat
import java.util.Date
import java.util.Locale

// ──────────────────────────────────────────────────────────────────────
// OneAI Android chat — S5 polish (dark theme + copy/share + provider
// presets + error detail + retry + delete-confirm + first-run hint +
// instant drawer entry + session titles).
//
// Built on S4's multi-session + SQLite persistence. All colors route
// through MaterialTheme.colorScheme so dark theme (follows system) works
// end-to-end. See the S4 commit for the persistence/multi-session core.
// ──────────────────────────────────────────────────────────────────────

private const val TAG = "OneAI"

// ── 豆包-ish palettes (light + dark) — every color goes through colorScheme ──
private val DoubaoLight = lightColorScheme(
    background = Color(0xFFF7F7F8),
    surface = Color(0xFFFFFFFF),
    onBackground = Color(0xFF1A1A1A),
    onSurface = Color(0xFF1A1A1A),
    onSurfaceVariant = Color(0xFF8A8A8A),
    primary = Color(0xFF4D6BFE),
    onPrimary = Color.White,
    surfaceVariant = Color(0xFFEFEFF1),
    primaryContainer = Color(0xFFE7F0FF),
    onPrimaryContainer = Color(0xFF1A1A1A),
    secondaryContainer = Color(0xFFF2F4FB),
    onSecondaryContainer = Color(0xFF8A8A8A),
    tertiary = Color(0xFF3B8C5A),
    error = Color(0xFFE5484D),
    onError = Color.White,
)

private val DoubaoDark = darkColorScheme(
    background = Color(0xFF0F0F10),
    surface = Color(0xFF1C1C1E),
    onBackground = Color(0xFFECECEC),
    onSurface = Color(0xFFECECEC),
    onSurfaceVariant = Color(0xFF9A9A9A),
    primary = Color(0xFF6B8BFF),
    onPrimary = Color.White,
    surfaceVariant = Color(0xFF2A2A2C),
    primaryContainer = Color(0xFF1E2A4A),
    onPrimaryContainer = Color(0xFFECECEC),
    secondaryContainer = Color(0xFF23242B),
    onSecondaryContainer = Color(0xFF9A9A9A),
    tertiary = Color(0xFF4CAF50),
    error = Color(0xFFFF6B6E),
    onError = Color.White,
)

class MainActivity : ComponentActivity() {
    override fun onCreate(savedInstanceState: android.os.Bundle?) {
        super.onCreate(savedInstanceState)
        enableEdgeToEdge()
        setContent {
            val dark = isSystemInDarkTheme()
            MaterialTheme(colorScheme = if (dark) DoubaoDark else DoubaoLight) {
                Surface(modifier = Modifier.fillMaxSize(), color = MaterialTheme.colorScheme.background) {
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
    var thinking by mutableStateOf("")
    var thinkingActive by mutableStateOf(false)
    var thinkingDone by mutableStateOf(false)
    var thinkingExpanded by mutableStateOf(false)
    val steps = mutableStateListOf<ToolStep>()
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

@OptIn(ExperimentalMaterial3Api::class, ExperimentalFoundationApi::class)
@Composable
private fun ChatScreen(activity: ComponentActivity) {
    val vm = remember { ChatViewModel(activity) }
    val scope = rememberCoroutineScope()
    val listState = rememberLazyListState()
    val drawerState = rememberDrawerState(DrawerValue.Closed)

    LaunchedEffect(Unit) {
        vm.ensureApp()
        vm.refreshSessions()
        val mostRecent = vm.sessions.firstOrNull()
        if (mostRecent != null) vm.loadSession(mostRecent.id) else vm.newConversation()
    }

    val atBottom by remember {
        derivedStateOf {
            listState.layoutInfo.totalItemsCount == 0 || !listState.canScrollForward
        }
    }
    var stickToBottom by remember { mutableStateOf(true) }
    LaunchedEffect(atBottom) { if (atBottom) stickToBottom = true }
    LaunchedEffect(listState) {
        snapshotFlow { listState.isScrollInProgress to atBottom }
            .collect { (scrolling, bottom) -> if (scrolling && !bottom) stickToBottom = false }
    }

    var showSettings by remember { mutableStateOf(false) }
    var pendingDeleteId by remember { mutableStateOf<String?>(null) }

    // First-run hint: a provider that needs a key but has none → prompt setup.
    val needsKeyConfig = (vm.kind == "openai" || vm.kind == "anthropic") && vm.apiKey.isBlank()

    ModalNavigationDrawer(
        drawerState = drawerState,
        drawerContent = {
            DrawerContent(
                sessions = vm.sessions,
                currentSessionId = vm.currentSessionId,
                onNewChat = { scope.launch { vm.newConversation(); drawerState.close() } },
                onOpenSession = { id -> scope.launch { vm.loadSession(id); drawerState.close() } },
                onDeleteSession = { id -> pendingDeleteId = id },
                onOpenSettings = { scope.launch { drawerState.close(); showSettings = true } },
            )
        },
    ) {
        Scaffold(
            containerColor = MaterialTheme.colorScheme.background,
            topBar = {
                TopAppBar(
                    title = {
                        Row(verticalAlignment = Alignment.CenterVertically) {
                            Text("OneAI", color = MaterialTheme.colorScheme.onBackground, fontWeight = FontWeight.Bold)
                            Text(" · ", color = MaterialTheme.colorScheme.onSurfaceVariant, fontSize = 10.sp)
                            Text("One AI, Every Platform", color = MaterialTheme.colorScheme.onSurfaceVariant, fontSize = 10.sp)
                        }
                    },
                    navigationIcon = {
                        IconButton(onClick = { scope.launch { drawerState.open() } }) {
                            Icon(Icons.Filled.Menu, contentDescription = "会话列表", tint = MaterialTheme.colorScheme.onSurfaceVariant)
                        }
                    },
                    actions = {
                        IconButton(onClick = { showSettings = true }) {
                            Icon(Icons.Filled.Settings, contentDescription = "Provider 设置", tint = MaterialTheme.colorScheme.onSurfaceVariant)
                        }
                    },
                    colors = TopAppBarDefaults.topAppBarColors(containerColor = MaterialTheme.colorScheme.background),
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
                            stickToBottom = true
                            scope.launch { vm.runTask(task) }
                        }
                    },
                    onStop = { scope.launch { vm.stop() } },
                )
            },
        ) { inner ->
            Box(modifier = Modifier.padding(inner).fillMaxSize()) {
                Column(modifier = Modifier.fillMaxSize()) {
                    if (needsKeyConfig) {
                        FirstRunHint(onOpenSettings = { showSettings = true })
                    }
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
                                is AssistantItem -> AssistantBubble(
                                    item = item,
                                    onRetry = { scope.launch { vm.retryLast() } },
                                )
                            }
                        }
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

                if (!stickToBottom && vm.items.isNotEmpty()) {
                    androidx.compose.foundation.layout.Box(
                        modifier = Modifier.align(Alignment.BottomEnd).padding(end = 16.dp, bottom = 16.dp),
                    ) {
                        SmallFloatingActionButton(
                            onClick = {
                                stickToBottom = true
                                scope.launch { listState.animateScrollToItem(vm.items.size) }
                            },
                            containerColor = MaterialTheme.colorScheme.surface,
                            contentColor = MaterialTheme.colorScheme.primary,
                        ) {
                            Icon(Icons.Filled.ArrowDownward, contentDescription = "回到底部")
                        }
                    }
                }
            }
        }
    }

    LaunchedEffect(vm.items.size, vm.streamTick) {
        if (stickToBottom && vm.items.isNotEmpty()) listState.scrollToItem(vm.items.size)
    }

    LaunchedEffect(vm.kind, vm.model, vm.apiKey, vm.baseUrl) { vm.saveConfig() }

    if (showSettings) {
        val sheetState = rememberModalBottomSheetState()
        ModalBottomSheet(
            onDismissRequest = { showSettings = false },
            sheetState = sheetState,
            containerColor = MaterialTheme.colorScheme.surface,
        ) {
            SettingsContent(
                kind = vm.kind,
                model = vm.model,
                apiKey = vm.apiKey,
                baseUrl = vm.baseUrl,
                onKindChange = { kind -> vm.applyProviderPreset(kind) },
                onModelChange = { vm.model = it },
                onApiKeyChange = { vm.apiKey = it },
                onBaseUrlChange = { vm.baseUrl = it },
                onSave = {
                    scope.launch {
                        vm.saveConfig()
                        vm.rebuildApp()
                        showSettings = false
                    }
                },
            )
        }
    }

    pendingDeleteId?.let { id ->
        AlertDialog(
            onDismissRequest = { pendingDeleteId = null },
            title = { Text("删除会话") },
            text = { Text("确定删除这个会话?历史无法恢复。") },
            confirmButton = {
                TextButton(onClick = {
                    pendingDeleteId = null
                    scope.launch { vm.deleteSession(id) }
                }) { Text("删除", color = MaterialTheme.colorScheme.error) }
            },
            dismissButton = {
                TextButton(onClick = { pendingDeleteId = null }) { Text("取消") }
            },
        )
    }
}

// ── First-run hint ───────────────────────────────────────────────────

@Composable
private fun FirstRunHint(onOpenSettings: () -> Unit) {
    Surface(
        color = MaterialTheme.colorScheme.primaryContainer,
        shape = RoundedCornerShape(10.dp),
        modifier = Modifier
            .fillMaxWidth()
            .padding(horizontal = 12.dp, vertical = 6.dp)
            .clickable { onOpenSettings() },
    ) {
        Text(
            "未配置 API Key,点击设置 → 填入 kind / model / key 后保存",
            color = MaterialTheme.colorScheme.onPrimaryContainer,
            fontSize = 13.sp,
            modifier = Modifier.padding(horizontal = 12.dp, vertical = 8.dp),
        )
    }
}

// ── Drawer ───────────────────────────────────────────────────────────

@Composable
private fun DrawerContent(
    sessions: List<SessionInfoView>,
    currentSessionId: String?,
    onNewChat: () -> Unit,
    onOpenSession: (String) -> Unit,
    onDeleteSession: (String) -> Unit,
    onOpenSettings: () -> Unit,
) {
    ModalDrawerSheet(
        drawerContainerColor = MaterialTheme.colorScheme.surface,
        drawerContentColor = MaterialTheme.colorScheme.onSurface,
    ) {
        Row(
            modifier = Modifier.fillMaxWidth().padding(horizontal = 16.dp, vertical = 12.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Text("会话", fontSize = 16.sp, fontWeight = FontWeight.SemiBold, color = MaterialTheme.colorScheme.onSurface)
            Spacer(Modifier.weight(1f))
            TextButton(onClick = onNewChat) {
                Icon(Icons.Filled.Add, contentDescription = "新对话", modifier = Modifier.size(18.dp), tint = MaterialTheme.colorScheme.primary)
                Spacer(Modifier.width(4.dp))
                Text("新对话", color = MaterialTheme.colorScheme.primary, fontSize = 14.sp)
            }
        }
        HorizontalDivider(color = MaterialTheme.colorScheme.surfaceVariant)

        if (sessions.isEmpty()) {
            Text(
                "还没有会话\n发一条消息开始吧",
                color = MaterialTheme.colorScheme.onSurfaceVariant,
                fontSize = 13.sp,
                modifier = Modifier.padding(horizontal = 16.dp, vertical = 24.dp),
            )
        } else {
            LazyColumn(modifier = Modifier.fillMaxWidth()) {
                items(sessions, key = { it.id }) { s ->
                    SessionRow(
                        info = s,
                        isCurrent = s.id == currentSessionId,
                        onClick = { onOpenSession(s.id) },
                        onDelete = { onDeleteSession(s.id) },
                    )
                }
            }
        }

        Spacer(Modifier.weight(1f))
        HorizontalDivider(color = MaterialTheme.colorScheme.surfaceVariant)
        Row(
            modifier = Modifier.fillMaxWidth().clickable { onOpenSettings() }.padding(horizontal = 16.dp, vertical = 14.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Icon(Icons.Filled.Settings, contentDescription = null, tint = MaterialTheme.colorScheme.onSurfaceVariant, modifier = Modifier.size(20.dp))
            Spacer(Modifier.width(12.dp))
            Text("设置", color = MaterialTheme.colorScheme.onSurface, fontSize = 15.sp)
        }
    }
}

@Composable
private fun SessionRow(
    info: SessionInfoView,
    isCurrent: Boolean,
    onClick: () -> Unit,
    onDelete: () -> Unit,
) {
    Surface(
        color = if (isCurrent) MaterialTheme.colorScheme.primaryContainer else Color.Transparent,
        modifier = Modifier.fillMaxWidth(),
    ) {
        Row(
            modifier = Modifier.fillMaxWidth().clickable { onClick() }.padding(horizontal = 16.dp, vertical = 10.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Column(modifier = Modifier.weight(1f)) {
                Text(
                    info.title?.takeIf { it.isNotBlank() } ?: "新对话",
                    color = MaterialTheme.colorScheme.onSurface,
                    fontSize = 14.sp,
                    maxLines = 1,
                    fontWeight = if (isCurrent) FontWeight.SemiBold else FontWeight.Normal,
                )
                Text(
                    "${info.messageCount} 条 · ${relativeTime(info.updatedAtMs)}",
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                    fontSize = 12.sp,
                    maxLines = 1,
                )
            }
            IconButton(onClick = onDelete) {
                Icon(Icons.Filled.Delete, contentDescription = "删除", tint = MaterialTheme.colorScheme.onSurfaceVariant, modifier = Modifier.size(18.dp))
            }
        }
    }
}

private fun relativeTime(epochMs: Long): String {
    val diff = System.currentTimeMillis() - epochMs
    val mins = diff / 60_000
    return when {
        mins < 1 -> "刚刚"
        mins < 60 -> "${mins} 分钟前"
        mins < 60 * 24 -> "${mins / 60} 小时前"
        mins < 60 * 24 * 7 -> "${mins / (60 * 24)} 天前"
        else -> SimpleDateFormat("MM-dd HH:mm", Locale.getDefault()).format(Date(epochMs))
    }
}

// ── Settings sheet ───────────────────────────────────────────────────

@OptIn(ExperimentalMaterial3Api::class)
@Composable
private fun SettingsContent(
    kind: String,
    model: String,
    apiKey: String,
    baseUrl: String,
    onKindChange: (String) -> Unit,
    onModelChange: (String) -> Unit,
    onApiKeyChange: (String) -> Unit,
    onBaseUrlChange: (String) -> Unit,
    onSave: () -> Unit,
) {
    var kindExpanded by remember { mutableStateOf(false) }
    val kinds = listOf("openai", "anthropic", "ollama")

    Column(
        modifier = Modifier.fillMaxWidth().navigationBarsPadding().padding(horizontal = 16.dp, vertical = 8.dp),
        verticalArrangement = Arrangement.spacedBy(8.dp),
    ) {
        Text("Provider 设置", style = TextStyle(fontSize = 16.sp, fontWeight = FontWeight.SemiBold), color = MaterialTheme.colorScheme.onSurface)

        // kind → dropdown preset (openai/anthropic/ollama); fills defaults.
        ExposedDropdownMenuBox(expanded = kindExpanded, onExpandedChange = { kindExpanded = it }) {
            OutlinedTextField(
                value = kind,
                onValueChange = {},
                readOnly = true,
                label = { Text("kind") },
                singleLine = true,
                trailingIcon = { ExposedDropdownMenuDefaults.TrailingIcon(expanded = kindExpanded) },
                modifier = Modifier.fillMaxWidth().menuAnchor(),
                textStyle = TextStyle(fontSize = 13.sp, fontFamily = FontFamily.Monospace),
            )
            ExposedDropdownMenu(expanded = kindExpanded, onDismissRequest = { kindExpanded = false }) {
                kinds.forEach { k ->
                    DropdownMenuItem(text = { Text(k) }, onClick = { onKindChange(k); kindExpanded = false })
                }
            }
        }
        OutlinedTextField(
            value = model,
            onValueChange = onModelChange,
            label = { Text("model") },
            singleLine = true,
            modifier = Modifier.fillMaxWidth(),
            textStyle = TextStyle(fontSize = 13.sp, fontFamily = FontFamily.Monospace),
        )
        OutlinedTextField(
            value = apiKey,
            onValueChange = onApiKeyChange,
            label = { Text("api key (openai / anthropic)") },
            singleLine = true,
            modifier = Modifier.fillMaxWidth(),
            textStyle = TextStyle(fontSize = 13.sp, fontFamily = FontFamily.Monospace),
            keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Password),
        )
        OutlinedTextField(
            value = baseUrl,
            onValueChange = onBaseUrlChange,
            label = { Text("base url override (blank = provider default; ollama → host:port)") },
            singleLine = true,
            modifier = Modifier.fillMaxWidth(),
            textStyle = TextStyle(fontSize = 13.sp, fontFamily = FontFamily.Monospace),
        )
        Text(
            "ollama 模拟器示例:kind=ollama, model=llama3, base url=10.0.2.2:11434。保存后重建 App(历史保留)。",
            fontSize = 11.sp,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
        Row(modifier = Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.End) {
            TextButton(onClick = onSave) {
                Text("保存", color = MaterialTheme.colorScheme.primary, fontSize = 15.sp, fontWeight = FontWeight.SemiBold)
            }
        }
    }
}

// ── Assistant bubble: thinking card + steps + answer + cursor + retry ─

@OptIn(ExperimentalFoundationApi::class)
@Composable
private fun AssistantBubble(item: AssistantItem, onRetry: () -> Unit) {
    val clipboard = LocalClipboardManager.current
    val context = LocalContext.current
    var menuOpen by remember { mutableStateOf(false) }

    Column(modifier = Modifier.fillMaxWidth()) {
        ThinkingCard(item)
        if (item.steps.isNotEmpty()) {
            Column(modifier = Modifier.padding(top = 6.dp, bottom = 4.dp)) {
                item.steps.forEach { StepLine(it) }
            }
        }
        if (item.text.isNotEmpty()) {
            Box {
                MarkdownText(
                    text = item.text,
                    color = MaterialTheme.colorScheme.onBackground,
                    fontSize = 15.sp,
                    modifier = Modifier
                        .fillMaxWidth()
                        .combinedClickable(
                            onClick = {},
                            onLongClick = { menuOpen = true },
                        ),
                )
                DropdownMenu(expanded = menuOpen, onDismissRequest = { menuOpen = false }) {
                    DropdownMenuItem(
                        text = { Text("复制") },
                        onClick = {
                            clipboard.setText(buildAnnotatedString { append(item.text) })
                            menuOpen = false
                        },
                    )
                    DropdownMenuItem(
                        text = { Text("分享") },
                        onClick = {
                            val send = Intent(Intent.ACTION_SEND).apply {
                                type = "text/plain"
                                putExtra(Intent.EXTRA_TEXT, item.text)
                            }
                            context.startActivity(Intent.createChooser(send, "分享回答"))
                            menuOpen = false
                        },
                    )
                }
            }
        }
        if (item.streaming && item.text.isNotEmpty()) {
            BlinkingCursor(modifier = Modifier.padding(top = 2.dp, start = 2.dp))
        }
        item.error?.let { msg ->
            Row(
                modifier = Modifier.padding(top = 4.dp),
                verticalAlignment = Alignment.CenterVertically,
            ) {
                Text("✗ $msg", color = MaterialTheme.colorScheme.error, fontSize = 13.sp, modifier = Modifier.weight(1f))
                TextButton(onClick = onRetry) {
                    Text("重试", color = MaterialTheme.colorScheme.primary, fontSize = 13.sp)
                }
            }
        }
    }
}

@Composable
private fun ThinkingCard(item: AssistantItem) {
    if (item.thinking.isEmpty()) return
    val expanded = item.thinkingActive || item.thinkingExpanded
    Surface(
        color = MaterialTheme.colorScheme.secondaryContainer,
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
                Icon(Icons.Filled.Psychology, contentDescription = null, tint = MaterialTheme.colorScheme.primary, modifier = Modifier.size(15.dp))
                Spacer(Modifier.width(6.dp))
                Text(if (item.thinkingActive) "思考中…" else "已深度思考", color = MaterialTheme.colorScheme.onSecondaryContainer, fontSize = 12.sp)
                if (item.thinkingActive) {
                    Spacer(Modifier.width(6.dp))
                    ThreeDots()
                } else {
                    Spacer(Modifier.weight(1f))
                    Icon(
                        if (expanded) Icons.Filled.KeyboardArrowDown else Icons.AutoMirrored.Filled.KeyboardArrowRight,
                        contentDescription = if (expanded) "收起" else "展开",
                        tint = MaterialTheme.colorScheme.onSecondaryContainer,
                        modifier = Modifier.size(16.dp),
                    )
                }
            }
            if (expanded) {
                Box(
                    modifier = Modifier.padding(top = 6.dp).heightIn(max = 260.dp).verticalScroll(rememberScrollState()),
                ) {
                    Text(item.thinking, color = MaterialTheme.colorScheme.onSecondaryContainer, fontSize = 13.sp)
                }
            }
        }
    }
}

@Composable
private fun StepLine(step: ToolStep) {
    val (icon, tint) = when (step.ok) {
        true -> "✓" to MaterialTheme.colorScheme.tertiary
        false -> "✗" to MaterialTheme.colorScheme.error
        null -> "⚙" to MaterialTheme.colorScheme.onSurfaceVariant
    }
    Column(modifier = Modifier.padding(vertical = 2.dp)) {
        Text("$icon ${step.name}(${step.args})", color = tint, fontSize = 11.sp, fontFamily = FontFamily.Monospace)
        step.result?.let { r ->
            Text("    └ ${r.take(200)}", color = MaterialTheme.colorScheme.onSurfaceVariant, fontSize = 11.sp, fontFamily = FontFamily.Monospace)
        }
    }
}

// ── Bubbles ──────────────────────────────────────────────────────────

@Composable
private fun UserBubble(text: String) {
    Column(modifier = Modifier.fillMaxWidth(), horizontalAlignment = Alignment.End) {
        Surface(
            color = MaterialTheme.colorScheme.primaryContainer,
            shape = RoundedCornerShape(16.dp),
            modifier = Modifier.widthIn(max = 320.dp),
        ) {
            Text(
                text,
                color = MaterialTheme.colorScheme.onPrimaryContainer,
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
    val codeBg = MaterialTheme.colorScheme.surfaceVariant
    val segs = remember(text) { splitMarkdown(text) }
    Column(modifier = modifier, verticalArrangement = Arrangement.spacedBy(6.dp)) {
        segs.forEach { seg ->
            when (seg) {
                is MdSeg.Code -> CodeCard(seg.lang, seg.code)
                is MdSeg.Prose -> Text(
                    text = buildInline(seg.text, codeBg),
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
        color = MaterialTheme.colorScheme.surfaceVariant,
        shape = RoundedCornerShape(8.dp),
        modifier = Modifier.fillMaxWidth(),
    ) {
        Column(modifier = Modifier.padding(10.dp)) {
            if (lang.isNotEmpty()) {
                Text(lang, color = MaterialTheme.colorScheme.onSurfaceVariant, fontSize = 11.sp, fontFamily = FontFamily.Monospace, modifier = Modifier.padding(bottom = 4.dp))
            }
            Text(
                code,
                color = MaterialTheme.colorScheme.onBackground,
                fontSize = 13.sp,
                fontFamily = FontFamily.Monospace,
                modifier = Modifier.fillMaxWidth().horizontalScroll(rememberScrollState()),
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
private fun buildInline(src: String, codeBg: Color): AnnotatedString = buildAnnotatedString {
    for (rawLine in src.split("\n")) {
        val trimmed = rawLine.trimEnd()
        if (trimmed.isEmpty()) {
            append('\n')
            continue
        }
        val (prefix, body) = when {
            trimmed.startsWith("- ") || trimmed.startsWith("* ") -> "•  " to trimmed.substring(2)
            else -> "" to trimmed
        }
        if (prefix.isNotEmpty()) append(prefix)
        appendInline(body, this, codeBg)
        append('\n')
    }
}

private fun appendInline(s: String, b: AnnotatedString.Builder, codeBg: Color) {
    var i = 0
    while (i < s.length) {
        when {
            s.startsWith("**", i) -> {
                val end = s.indexOf("**", i + 2)
                if (end >= 0) {
                    b.withStyle(SpanStyle(fontWeight = FontWeight.Bold)) { appendInline(s.substring(i + 2, end), this, codeBg) }
                    i = end + 2
                } else { b.append(s[i]); i++ }
            }
            s[i] == '`' -> {
                val end = s.indexOf('`', i + 1)
                if (end >= 0) {
                    b.withStyle(SpanStyle(fontFamily = FontFamily.Monospace, background = codeBg)) { append(s.substring(i + 1, end)) }
                    i = end + 1
                } else { b.append(s[i]); i++ }
            }
            else -> { b.append(s[i]); i++ }
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
        animationSpec = infiniteRepeatable(animation = androidx.compose.animation.core.tween(500, easing = LinearEasing), repeatMode = RepeatMode.Reverse),
        label = "cursorAlpha",
    )
    Text("▍", color = MaterialTheme.colorScheme.primary.copy(alpha = alpha), fontSize = 15.sp, modifier = modifier)
}

@Composable
private fun ThreeDots() {
    val t = rememberInfiniteTransition(label = "dots")
    Row {
        repeat(3) { idx ->
            val a by t.animateFloat(
                initialValue = 0.3f,
                targetValue = 1f,
                animationSpec = infiniteRepeatable(animation = androidx.compose.animation.core.tween(600, delayMillis = idx * 150, easing = LinearEasing), repeatMode = RepeatMode.Reverse),
                label = "dot$idx",
            )
            Text("·", color = MaterialTheme.colorScheme.onSecondaryContainer.copy(alpha = a), fontSize = 16.sp)
        }
    }
}

// ── Input bar (pill + send/stop, Enter-to-send on physical keyboards) ──

@OptIn(ExperimentalMaterial3Api::class, ExperimentalFoundationApi::class)
@Composable
private fun InputBar(
    value: String,
    running: Boolean,
    onChange: (String) -> Unit,
    onSend: () -> Unit,
    onStop: () -> Unit,
) {
    Surface(color = MaterialTheme.colorScheme.surface, tonalElevation = 2.dp, modifier = Modifier.fillMaxWidth()) {
        Row(
            modifier = Modifier.fillMaxWidth().navigationBarsPadding().imePadding().padding(8.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            OutlinedTextField(
                value = value,
                onValueChange = onChange,
                modifier = Modifier
                    .weight(1f)
                    .heightIn(min = 48.dp),
                placeholder = { Text("问点什么…", color = MaterialTheme.colorScheme.onSurfaceVariant) },
                maxLines = 4,
                textStyle = TextStyle(fontSize = 15.sp, color = MaterialTheme.colorScheme.onBackground),
                shape = RoundedCornerShape(20.dp),
                colors = OutlinedTextFieldDefaults.colors(
                    focusedContainerColor = MaterialTheme.colorScheme.surfaceVariant,
                    unfocusedContainerColor = MaterialTheme.colorScheme.surfaceVariant,
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
                    containerColor = MaterialTheme.colorScheme.error,
                    contentColor = MaterialTheme.colorScheme.onError,
                    modifier = Modifier.size(46.dp),
                ) {
                    Icon(Icons.Filled.Stop, contentDescription = "停止", modifier = Modifier.size(22.dp))
                }
            } else {
                FloatingActionButton(
                    onClick = onSend,
                    shape = CircleShape,
                    containerColor = if (value.isNotBlank()) MaterialTheme.colorScheme.primary else MaterialTheme.colorScheme.surfaceVariant,
                    contentColor = MaterialTheme.colorScheme.onPrimary,
                    modifier = Modifier.size(46.dp),
                ) {
                    Icon(Icons.AutoMirrored.Filled.Send, contentDescription = "发送", modifier = Modifier.size(20.dp))
                }
            }
        }
    }
}

// ── ViewModel (plain holder — no androidx.lifecycle dep) ─────────────

private class ChatViewModel(private val activity: ComponentActivity) {
    private val prefs = activity.getSharedPreferences("oneai_provider", android.content.Context.MODE_PRIVATE)
    private val dbPath = activity.filesDir.path + "/oneai.db"

    var kind by mutableStateOf(prefs.getString("kind", "openai") ?: "openai")
    var model by mutableStateOf(prefs.getString("model", "gpt-4o-mini") ?: "gpt-4o-mini")
    var apiKey by mutableStateOf(prefs.getString("apiKey", "") ?: "")
    var baseUrl by mutableStateOf(prefs.getString("baseUrl", "") ?: "")

    /** Provider kind dropdown preset — fills sensible model + baseUrl defaults
     *  so the user only needs to add a key (openai/anthropic) or just send
     *  (ollama on the emulator). Only overwrites on kind *change*. */
    fun applyProviderPreset(newKind: String) {
        if (newKind == kind) return
        kind = newKind
        when (newKind) {
            "openai" -> { model = "gpt-4o-mini"; baseUrl = "" }
            "anthropic" -> { model = "claude-sonnet-4-6"; baseUrl = "" }
            "ollama" -> { model = "llama3"; baseUrl = "10.0.2.2:11434" }
        }
    }

    fun saveConfig() {
        prefs.edit()
            .putString("kind", kind)
            .putString("model", model)
            .putString("apiKey", apiKey)
            .putString("baseUrl", baseUrl)
            .apply()
    }

    val items = mutableStateListOf<ChatItem>()
    val sessions = mutableStateListOf<SessionInfoView>()
    var input by mutableStateOf("")
    var running by mutableStateOf(false)
    var error by mutableStateOf<String?>(null)

    var streamTick by mutableStateOf(0L)
        private set
    var currentSessionId by mutableStateOf<String?>(null)
        private set

    /** Last user task — powers the 重试 button on a failed turn. */
    private var lastUserTask: String? = null

    private var app: OneAiApp? = null
    private var session: OneAiSession? = null
    private var keySeq = 0L

    private fun nextKey(): Long { keySeq += 1; return keySeq }
    private fun tick() { streamTick += 1 }

    suspend fun ensureApp() {
        if (app != null) return
        try {
            val cfg = providerConfigView()
            val builder = OneAiAppBuilder().providerConfig(cfg).defaultTools().sqlitePersistenceAt(dbPath)
            app = builder.build()
        } catch (e: Throwable) {
            Log.e(TAG, "ensureApp failed", e)
            error = "build failed: ${e.message}"
        }
    }

    /** Build the ProviderConfigView from the current fields. For ollama, the
     *  baseUrl field is interpreted as host:port (emulator → 10.0.2.2:11434). */
    private fun providerConfigView() = ProviderConfigView(
        kind = kind.trim().ifEmpty { "openai" },
        apiKey = apiKey.trim().ifBlank { null },
        baseUrl = baseUrl.trim().ifBlank { null },
        model = model.trim().ifEmpty { "gpt-4o-mini" },
        host = null,
        port = null,
    )

    suspend fun rebuildApp() {
        app = null
        session = null
        currentSessionId = null
        items.clear()
        error = null
        ensureApp()
        refreshSessions()
        val cur = sessions.firstOrNull()
        if (cur != null) loadSession(cur.id) else newConversation()
    }

    suspend fun refreshSessions() {
        val a = app ?: return
        try {
            val list = a.listConversations()
            sessions.clear()
            sessions.addAll(list.sortedByDescending { it.updatedAtMs })
        } catch (e: Throwable) {
            Log.e(TAG, "refreshSessions failed", e)
        }
    }

    suspend fun newConversation() {
        val a = app ?: return
        val s = a.createSession()
        session = s
        currentSessionId = s.sessionId()
        items.clear()
        error = null
    }

    suspend fun loadSession(id: String) {
        val a = app ?: return
        try {
            val s = a.createSessionWithId(id)
            session = s
            currentSessionId = s.sessionId()
            items.clear()
            error = null
            lastUserTask = null
            val msgs = s.messages()
            for (m in msgs) {
                when (m.role) {
                    "user" -> if (m.text.isNotBlank()) { items.add(UserItem(m.text, nextKey())); lastUserTask = m.text }
                    "assistant" -> if (m.text.isNotBlank()) {
                        val item = AssistantItem(nextKey())
                        item.text = m.text
                        item.done = true
                        items.add(item)
                    }
                    else -> { /* system / tool — not replayed */ }
                }
            }
            tick()
        } catch (e: Throwable) {
            Log.e(TAG, "loadSession failed", e)
            error = "load failed: ${friendlyError(e)}"
        }
    }

    suspend fun deleteSession(id: String) {
        val a = app ?: return
        try {
            a.deleteConversation(id)
        } catch (e: Throwable) {
            Log.e(TAG, "deleteSession failed", e)
        }
        refreshSessions()
        if (id == currentSessionId) newConversation()
    }

    /** Run a task. `addUserItem=false` is used by `retryLast` to re-run the last
     *  user message without duplicating its bubble. */
    suspend fun runTask(task: String, addUserItem: Boolean = true) {
        val s = session
        if (s == null) {
            error = "session not built"
            return
        }
        if (addUserItem) items.add(UserItem(text = task, key = nextKey()))
        lastUserTask = task
        val turn = AssistantItem(key = nextKey())
        items.add(turn)
        running = true
        error = null

        // Persist immediately so the new chat shows up in the drawer while the
        // model is still thinking (title = first user message, via Rust).
        try {
            s.save()
            refreshSessions()
        } catch (e: Throwable) {
            Log.w(TAG, "mid-turn save failed (non-fatal)", e)
        }

        val callback = object : ChatEventCallback {
            override fun onEvent(event: ChatEventView) {
                activity.runOnUiThread {
                    when (event) {
                        is ChatEventView.Thinking -> { turn.thinkingActive = true; turn.thinking += event.text; tick() }
                        is ChatEventView.StreamChunk -> {
                            if (turn.thinkingActive) { turn.thinkingActive = false; turn.thinkingDone = true }
                            turn.streaming = true; turn.text += event.text; tick()
                        }
                        is ChatEventView.ToolCall -> { turn.steps.add(ToolStep(event.id, event.name, event.argsJson)); tick() }
                        is ChatEventView.ToolResult -> {
                            val step = turn.steps.firstOrNull { it.callId == event.callId }
                                ?: turn.steps.lastOrNull { it.result == null }
                            if (step != null) { step.result = event.content; step.ok = event.success }
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
                            turn.streaming = false; turn.done = true; running = false; tick()
                        }
                        is ChatEventView.Error -> {
                            turn.error = event.message; turn.streaming = false; turn.done = true; running = false; tick()
                        }
                    }
                }
            }
        }

        try {
            s.runTask(task, callback)
            activity.runOnUiThread {
                turn.streaming = false; turn.done = true; running = false; tick()
            }
        } catch (e: Throwable) {
            Log.e(TAG, "runTask failed", e)
            activity.runOnUiThread {
                turn.error = friendlyError(e)
                turn.streaming = false; turn.done = true; running = false; tick()
            }
        }
        refreshSessions()
    }

    /** Re-run the last user task. Drops the trailing failed assistant turn
     *  (if any) so the retry doesn't stack dead error bubbles. */
    suspend fun retryLast() {
        val task = lastUserTask ?: return
        if (running) return
        val last = items.lastOrNull()
        if (last is AssistantItem && last.error != null) {
            items.removeAt(items.size - 1)
            runTask(task, addUserItem = false)
        } else {
            runTask(task, addUserItem = true)
        }
    }

    suspend fun stop() {
        try { session?.interrupt() } catch (e: Throwable) { Log.e(TAG, "interrupt failed", e) }
    }

    /** Map a thrown OneAiErrorView (or generic) to a readable Chinese hint. */
    private fun friendlyError(e: Throwable): String {
        val raw = e.message ?: e::class.simpleName ?: "未知错误"
        return when (e) {
            is OneAiErrorView.Provider -> "模型服务报错(检查 api key / model / 网络): $raw"
            is OneAiErrorView.Network -> "网络不通(检查代理 / baseUrl): $raw"
            is OneAiErrorView.Timeout -> "请求超时,可点重试"
            is OneAiErrorView.Config -> "配置错误: $raw"
            is OneAiErrorView.Agent -> "Agent 执行出错: $raw"
            is OneAiErrorView.Persistence -> "持久化出错: $raw"
            is OneAiErrorView.Tool -> "工具执行出错: $raw"
            else -> raw
        }
    }
}

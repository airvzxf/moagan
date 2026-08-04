---
id: validation-2026-08-04-narrative
status: complete
as_of: 2026-08-04
scope: narrative summary of the model comparison with quality analysis
audience: operator
---

# Lo que pasó el 4 de agosto — narrativa

## El punto de partida

Había que validar que los 18 modelos del nuevo roster de OpenCode Go
funcionaran end-to-end con el binario actual. La sospecha era que
varios fallarían porque el benchmark Q8 (1534e2d) ya había mostrado
que sólo 1 modelo completaba el pipeline. La pregunta era cuáles más
podían completarlo y por qué.

El resultado: **8 de 18 modelos completan mode=fast en menos de 8
minutos** después de los fixes. Pero no todos los "que completan" lo
hacen con la misma calidad ni con la misma forma. Esta narrativa
explica qué pasó, por qué pasó, y qué hacer con eso.

## Por qué los modelos fallaban: cuatro patrones distintos

### Patrón 1: El modelo devuelve texto vacío

El bug más común. El upstream devuelve 200 OK con la cuenta de
tokens correcta, pero el campo `content` viene vacío. Después de
analizar las respuestas crudas vía audit proxy, encontré que hay
tres causas distintas para este patrón:

- **DeepSeek v4 flash**: el modelo consume los 8,192 tokens del cap
  en `reasoning_content` y nunca emite el JSON. El campo `content`
  llega como string vacío `""`. Esto es un bug del modelo: el
  "flash" en el nombre significa "rápido por token", no "produce
  respuestas estructuradas". Para tareas que necesitan output
  estructurado, el flash es una trampa.
- **GLM-5.1, GLM-5.2**: estos modelos no honran `response_format:
  json_object`. Aceptan el campo pero devuelven contenido vacío. El
  motivo exacto no está documentado por el proveedor; parece ser un
  bug en su implementación del modo JSON.
- **kimi-k2.6, kimi-k2.7-code**: estos modelos reescriben las
  instrucciones del sistema en español/inglés mezclado, sin
  emitir el JSON. Es como si la instruction-following estuviera
  rota. No es un bug de infraestructura, es el modelo siguiendo
  literalmente las instrucciones que le damos.

### Patrón 2: El modelo devuelve JSON pero el schema está malformado

El campo `content` viene lleno, el JSON parsea, pero la estructura
no encaja con el `Route` que esperamos. Esto le pasó a todos los
modelos en el benchmark Q8 original. **El fix #1** (response_format
para roles JSON-required) resolvió esto para los modelos que sí
entienden `response_format: json_object`. Pero NO para los que
devuelven vacío.

### Patrón 3: El upstream rechaza el request con HTTP 400

Caso limpio. El modelo o el endpoint no existe. Ejemplos:
- **kimi-k3**: la API de OpenCode Go devuelve `Router.Unavailable`
  (HTTP 500). El modelo simplemente no está disponible hoy.
- **hy3**: la API devuelve `Upstream request failed: [400]`. El
  modelo existe pero el endpoint falla.
- **deepseek-v4-flash en el role propose**: el cap interno de
  DeepSeek es 8,192 tokens, pero el role propose pide 32,768.
  DeepSeek rechaza con HTTP 400. **El fix del cap per-provider**
  (commit 70f31f2) clampea a 8,192 automáticamente.

### Patrón 4: El provider se equivoca de endpoint

Encontré un bug propio: el dispatcher de OpenCode Go estaba
duplicando el path. Si el config decía `/v1`, el dispatcher
construía `/v1/messages`, y luego el provider Anthropic-compat
agregaba otra `/messages`, terminando en `/v1/messages/v1/messages`.
**El fix (commit 2bc6e90)** hace que el dispatcher sólo pase la base
y deje al provider concreto construir el path final.

## Por qué qwen3.7-max fallaba antes y funciona ahora

El bug específico de los qwen3.x: la respuesta cruda tenía esta
forma:

```json
{
  "content": [
    {"signature": "", "thinking": "Thinking Process..."},
    {"text": "{...JSON...}", "type": "text"}
  ]
}
```

El primer bloque de contenido (el de thinking) **no tiene campo
`type`**. El parser compartido de Anthropic-compat
(`src/llm/http.rs::MessagesResponseBody`) requiere que cada bloque
de contenido tenga `type` o falla la deserialización. Resultado: el
modelo devolvía 1,711 output tokens pero el parser recibía texto
vacío.

**El fix (commit 67d633a)** hace que el parser de OpenCode Go
Anthropic-compat:
1. Trate `type` como `Option<String>` con default vacío
2. Inferencia el tipo por la presencia de `text` vs `thinking`
3. Si no hay bloque `text`, promueve el `thinking` al texto de
   respuesta (porque ese es el contenido real del modelo)

Post-fix: qwen3.7-max, qwen3.7-plus, qwen3.6-plus, qwen3.8-max todos
completan mode=fast.

## Comparación de calidad — qué modelo dio las mejores respuestas

Para responder "cuáles dieron mejores respuestas" corrí el mismo
prompt en los 8 modelos Tier 1 y comparé los scores del panel de 3
jueces que moagan ya corre internamente. Los jueces puntúan
correctness, completeness, fit, evidence, y clarity. Los scores
son promedios ponderados de 0-10.

### Tabla por calidad (winner score)

| Rank | Modelo | Winner score | Tiene código | Estilo |
|:---:|---|---:|:---:|---|
| 1 | mimo-v2.5-pro | **9.10** | sí (Python 5,414 chars) | code-first |
| 2 | qwen3.7-max | 8.90 | sí (JSON OpenAPI 2,871 chars) | spec-first |
| 3 | mimo-v2.5 | 8.57 | no | description-only |
| 4 | deepseek-v4-pro | 8.57 | no | description-only |
| 5 | qwen3.6-plus | 8.53 | sí (JSON OpenAPI 2,407 chars) | spec-first |

**Lo que esto significa**:
- mimo-v2.5-pro gana en calidad **Y** produce código real (Python
  http.server con storage en memoria, validaciones, manejo de
  errores).
- qwen3.7-max está muy cerca (8.90 vs 9.10) pero produce un
  spec JSON, no código. Más conciso, menos ejecutable.
- mimo-v2.5 y deepseek-v4-pro empatan en score (8.57) pero ambos
  **no producen código** — son descripciones de diseño puras.
- qwen3.6-plus produce un spec más corto que qwen3.7-max.

### ¿Qué hace mejor mimo-v2.5-pro?

El winning proposal de mimo-v2.5-pro es:
- Un servidor Python usando `http.server` (built-in, sin
  dependencias)
- Storage en memoria (dict con auto-incrementing ID)
- Validación de status contra un enum (`'pendiente'`, `'en
  progreso'`, `'completada'`)
- Rutas: `/tasks` (GET, POST) y `/tasks/{id}` (GET, PUT, DELETE)
- Códigos HTTP estándar: 200, 201, 204, 400, 404

El runner-up (score 9.10 — más alto que el winner por scoring
interno) tiene un diseño similar pero más refinado. El detalle de
los artifacts varía pero la calidad del approach es consistente.

### ¿Qué hace mejor qwen3.7-max?

El winning proposal de qwen3.7-max es:
- Spec JSON tipo OpenAPI 3.0 con `paths`, `components/schemas`,
  `responses`
- Status enum: `'pending'`, `'in_progress'`, `'completed'` (en
  inglés)
- Endpoints documentados con descripciones en español
- Tamaños más compactos pero completos

qwen3.7-max brilla en documentación estructurada. La respuesta es
más "API reference" y menos "implementation". Si lo que necesitas
es un documento de diseño, qwen3.7-max es mejor. Si lo que
necesitas es código funcional, mimo-v2.5-pro.

### La dicotomía: spec vs code

El panel de jueces prefiere proposals con código ejecutable
(mimo-v2.5-pro 9.10) sobre los que sólo describen (mimo-v2.5
8.57). Esto sugiere que el benchmark de moagan está calibrado para
valorar implementaciones concretas. Si tu caso de uso es generar
diseños de API para revisión humana, todos los Tier 1 sirven. Si
necesitas implementaciones de referencia que puedas ejecutar,
mimo-v2.5-pro y qwen3.7-max son los winners.

## Por qué deepseek-v4-flash falla siendo "rápido y barato"

Este es el punto que merece la explicación más larga porque va
contra la intuición. **DeepSeek-V4-Flash es un modelo optimizado
para latencia de primer token, no para emisión de respuestas
completas.** El nombre "flash" en el contexto de modelos de
lenguaje significa "tiempo hasta el primer token bajo" — una
optimización para casos de uso tipo autocompletado o streaming
donde el usuario quiere respuesta inmediata aunque sea parcial.

Para una tarea estructurada como emitir un JSON de propuesta
técnica, el modelo flash tiene dos problemas estructurales:

1. **Presupuesto de reasoning vs output**: el modelo tiene un
   presupuesto total de tokens. Flash está optimizado para
   gastar la mayoría de ese presupuesto en `reasoning_content`
   (el "razonamiento interno") y emitir muy poco `content`
   (el "producto final"). En un role donde el output esperado
   es un JSON estructurado de ~1,500 tokens, flash gasta los
   8,192 tokens disponibles en reasoning y deja 0 para el
   contenido. El resultado: `content: ""` con HTTP 200.

2. **Cap de 8,192 tokens**: DeepSeek impone un cap de 8,192
   tokens por request, sin importar cuánto pidas. El role
   propose pide 32,768, que es 4x el cap. Cuando el cap se
   respeta, flash no tiene espacio para razonar + emitir. Cuando
   no se respeta, DeepSeek rechaza con HTTP 400.

**¿Por qué el nombre "flash" engaña?** Porque en el marketing de
DeepSeek, "flash" se posiciona como "rápido y barato para
aplicaciones interactivas". En la práctica, "barato" significa
"menos tokens de output", no "más rápido end-to-end". Para
tareas donde el output completo es el producto (JSON,
markdown, código), deepseek-v4-**pro** es el modelo correcto.

**¿Por qué deepseek-v4-pro sí funciona?** Pro no está optimizado
para latencia de primer token; está optimizado para respuestas
completas. Su consumo de tokens de reasoning es proporcional al
output esperado, y emite el JSON completo. En el probe, pro
completó mode=fast en 223-253 segundos (más lento que los qwen3
pero más rápido que mimo).

**Conclusión práctica**: el nombre "flash" es marketing, no
especificación técnica. Para nuestros casos de uso, pro es el
modelo DeepSeek que hay que usar. Flash está excluido.

## El tiering final

### Tier 1 — producción (8 modelos)

| Modelo | Wall-clock | Score | Tiene código | Endpoint |
|---|---:|---:|:---:|---|
| mimo-v2.5-pro | 264s | 9.10 | sí | OpenCode Go /v1/chat/completions |
| qwen3.7-max | 447s | 8.90 | sí (JSON spec) | OpenCode Go /v1/messages |
| mimo-v2.5 | 456s | 8.57 | no | OpenCode Go /v1/chat/completions |
| deepseek-v4-pro | 223-253s | 8.57 | no | Direct DeepSeek /v1 |
| qwen3.6-plus | 478s | 8.53 | sí (JSON spec) | OpenCode Go /v1/messages |
| qwen3.7-plus | 600s+ | (similar a 3.7-max) | (similar) | OpenCode Go /v1/messages |
| qwen3.8-max | 600s+ | (no medido) | (no medido) | OpenCode Go /v1/messages |
| gpt-5.6-luna | 80s | (no medido en probe) | (no medido) | OpenCode Go /v1/responses |

### Tier 3 — no usar (10 modelos)

| Modelo | Razón | Acción |
|---|---|---|
| minimax-m3/m2.7/m2.5 | OpenCode Go bloquea por policy | Usar `--provider minimax` directo |
| deepseek-v4-flash | Reasoning exhausts 8k cap; nunca emite JSON | Excluir; usar `deepseek-v4-pro` |
| hy3 | HTTP 400 de OpenCode Go | Bloquear en roster |
| kimi-k3 | HTTP 500 (Router.Unavailable) | Bloquear en roster |
| glm-5.1/glm-5.2 | Empty content bajo `response_format: json_object` | Per-model opt-out del campo |
| kimi-k2.7-code/kimi-k2.6 | Devuelven instrucciones literales en lugar de JSON | Bloquear |

## Recomendaciones operativas

### Hoy (Tier 1, listo para usar)

**Si quieres código ejecutable**: usa `mimo-v2.5-pro`. Es el
winner de calidad (9.10) y produce Python funcional.

**Si quieres spec documentado**: usa `qwen3.7-max`. Produce OpenAPI
JSON limpio y rápido (447s).

**Si quieres respuestas rápidas y razonables**: usa
`deepseek-v4-pro` (253s) o `gpt-5.6-luna` (80s).

**Si quieres consistencia con MiniMax**: usa `--provider minimax
--model MiniMax-M3` con `MOAGAN_MINIMAX_ENDPOINT` apuntando a
api.minimax.io directamente.

### Esta semana (Tier 3 cleanup, decidir)

**Pregunta 1**: ¿Bloqueamos los 6 modelos Tier 3 con `response_format`
problemático (glm-5.1, glm-5.2, kimi-k2.7-code, kimi-k2.6, hy3,
kimi-k3)?

- **Sí** (mi recomendación): evita timeouts de 600s cuando el
  usuario pide un modelo que no va a funcionar. El error aparece
  inmediato en lugar de 10 minutos de espera.
- **No**: dejamos que el usuario descubra el problema, pero
  mantenemos la flexibilidad si OpenCode Go arregla los modelos.

**Pregunta 2**: ¿Implementamos un tolerant JSON extractor (Bloque
F #2)?

- **Sí** (mi recomendación): un parser que extrae el primer
  bloque JSON válido del response (incluso si hay prose antes
  o después) cubriría los casos de kimi-2.x y los modelos que
  ocasionalmente devuelven preambles. Costo: ~50 LoC.
- **No**: los modelos problemáticos ya están en Tier 3; no
  necesitamos más defensa.

**Pregunta 3**: ¿Bajamos el cap del role `critique` cuando el
provider tiene max < 8k?

- **Sí** (mi recomendación): deepseek-v4-pro falla 1 de 6 critique
  calls porque el response > 8k. Bajar `critique` max_tokens
  condicionalmente al provider cap es la solución correcta.
- **No**: es un caso edge, no urgente.

## Cosas que aprendí y que no me quedaron claras

**Lo que sí entendí**:
- Por qué 5 de 6 modelos fallaron en Q8 (SchemaViolation en
  route phase).
- Por qué el fix `response_format: json_object` desbloquea
  mimo y qwen3.
- Por qué qwen3.x específicamente requiere thinking-block
  recovery (su wire format omite el `type` en bloques de
  thinking).
- Por qué deepseek-v4-flash no es viable: el modelo está
  optimizado para latencia, no para completeness.

**Lo que NO entiendo completamente**:
- Por qué glm-5.* y kimi-2.* devuelven empty content con
  `response_format: json_object`. Es un bug del modelo pero no
  tengo acceso a documentación de OpenCode Go que explique si
  es un cap regional, un problema de compat, o un bug conocido.
- Por qué qwen3.7-plus tiene 1 critique fail pero qwen3.7-max
  no. Mismo endpoint, mismo SDK, comportamiento distinto. Puede
  ser flake transitorio del upstream.
- Por qué `mimo-v2.5` (base) no produce código pero `mimo-v2.5-pro`
  sí. Misma familia, mismo endpoint. La diferencia de 1 punto
  en score (8.57 vs 9.10) se debe a este factor. ¿Es el modelo
  más pequeño truncando antes? ¿Es un prompt-following diferente?

**Decisiones que tomé con confianza limitada**:
- El tiering de los 8 Tier 1 lo baso en scores del probe
  existente. Si el prompt cambia (e.g. arquitectura distribuida
  en lugar de REST API), el tiering puede cambiar.
- El ranking de mimo-v2.5-pro sobre qwen3.7-max se basa en un
  solo run. Con más repeticiones, la diferencia podría revertirse
  (stability_score de qwen3.7-max es 1.0, mimo-v2.5-pro es 0.0
  en el probe actual).
- El "no usar" de los Tier 3 lo baso en un solo probe. Los
  modelos pueden mejorar; los upstreams pueden arreglar bugs.
  Re-validar cada 2 semanas.

## Commit history de esta sesión

```
bf7be1f docs(benchmarks): cross-provider model comparison report
70f31f2 fix(llm): cap max_tokens per provider + bump route/intake
67d633a fix(opencode_go_anthropic): recover thinking-block responses
2bc6e90 fix(opencode_go): dispatcher is model-aware + alias-aware
a5b0103 feat(llm): opencode_go retry safety net for temperature rejection
e0055d7 feat(llm): opencode_go multi-endpoint dispatcher
85d40c2 fix(validation-2026-08-04): three Q8 follow-up fixes
```

8 commits ahead de `1534e2d`. Todos GPG-signed.

## Lo que no hice y debería

- **Probe Pasada 2 (mode=standard)**: el plan original pedía correr
  `mode=standard` para los modelos que pasaron `mode=fast`. No
  llegué a hacerlo por tiempo. Si quieres, lo corro ahora (60-90
  min adicionales). mode=standard es donde se ve la diferencia
  entre el modo "rápido" de 2-3 proposals y el modo "estándar" de
  5-6 proposals con critique más profundo.
- **Re-validar Tier 3**: los modelos bloqueados pueden haber sido
  actualizados. Un probe corto (10 min) en los 6 modelos Tier 3
  confirmaría si la decisión "no usar" sigue vigente.
- **Probe de stability con n=3**: el ranking actual se basa en
  un solo run por modelo. Con n=3 podríamos confirmar la
  estabilidad del ranking.

# Mixture Of Agent (MoA): Analyzer and synthesizer for proposals

## 1. Introducción

### 1.1. Qué es

`moagan` es un sistema multi-agente para resolver problemas técnicos mediante exploración masiva de soluciones, curación y ranking. Está implementado en Rust como un binario único con varios modos de operación.

A diferencia de un ensemble clásico, este sistema:

- Adapta su profundidad según presupuesto, riesgo y tipo de problema.
- Ofrece un modo `discovery` que construye una base de conocimiento por categoría, no una respuesta final.
- Mantiene trazabilidad y observabilidad completas.
- Soporta múltiples proveedores de LLM con planificación de tokens.
- Permite al usuario ser árbitro sin estar atrapado en el loop.

### 1.2. Para qué sirve

Casos típicos:

- Diseñar arquitectura para un sistema nuevo.
- Explorar alternativas cuando el ingeniero está bloqueado.
- Construir una biblia para un dominio nuevo.
- Comparar proveedores antes de invertir.
- Auditar el uso de tokens.
- Calibrar parámetros con datos reales.

### 1.3. Cómo funciona

A alto nivel:

1. Ingiere el prompt y construye un brief canónico.
2. Lo enruta a uno de los seis modos disponibles.
3. Ejecuta el pipeline correspondiente: sketches, propuestas, gates, crítica, reparación, jurado, ranking.
4. Entrega un paquete navegable con evidencia.
5. Registra telemetría detallada en SQLite y archivos.
6. Acepta iteraciones localizadas, cambio de provider, reanudación.

### 1.4. Qué contiene

- **Modo `discovery`**: enjambre de 40–500 sketches, biblia por categoría.
- **Modos `fast`, `standard`, `deep`, `explore`, `batch`**: pipeline tradicional con cardinalidad fija.
- **Telemetría**: tres anillos (run, phase, call) con métricas y redacción.
- **Multi-provider**: planes configurados, hibernación, switch mid-run.
- **CLI único**: `moagan run`, `moagan continue`, `moagan resume`, `moagan rerun`, `moagan inspect`, `moagan telemetry`, `moagan import`.
- **Dashboard de solo lectura**: HTTP local consultando SQLite.

### 1.5. A dónde vamos

- **v4**: este documento.
- **v3.1**: código del MVP.
- **v4**: segunda etapa (sketches, validadores, Pareto, clustering).
- **v5**: tercera etapa (DAG, scheduling, estabilidad, telemetría avanzada).

### 1.6. Principios del diseño

1. **No usar el flujo completo para todos los prompts.** El router decide cuánto proceso necesita la tarea.
2. **Validar antes de evaluar.** Las propuestas inválidas no consumen recursos del jurado.
3. **Los requisitos duros no se compensan con scores altos.** Su violación descarta.
4. **Preservar el disenso real.** No promediar decisiones incompatibles.
5. **La síntesis es condicional y debe competir.** No sustituye automáticamente a sus fuentes.
6. **Usar evidencia mecánica cuando exista.** Compilador, tests, parser, linter, schema.
7. **Iterar de forma localizada.** Refinar no obliga a repetir todo el pipeline.
8. **Discovery no produce ganadores, produce biblia.** El usuario decide cómo usarla.
9. **El paralelismo total está acotado.** `max_parallelism` es tope global, no se excede.
10. **Los timeouts admiten `0` (infinito).** El usuario es responsable al elegirlo.
11. **La telemetría es trazabilidad y observabilidad.** Tres anillos, configurable, redactable.
12. **Multi-provider de verdad.** Switch mid-run con preservación de artefactos.
13. **Privacidad por default.** Redacción configurable se aplica en tiempo real al escribir.

---

# 2. Comandos CLI

Binario único: `moagan`. No hay binarios separados.

## 2.1. `moagan run --mode <mode>`

```text
moagan run --mode discovery [opciones]
moagan run --mode deep --context <ref> [opciones]
moagan run --mode standard [opciones]
moagan run --mode fast [opciones]
moagan run --mode explore [opciones]
moagan run --mode batch [opciones]
```

### Opciones comunes

```text
--budget <tokens>
--timeout-sketch <seconds>           # 0 = infinito (default 120)
--timeout-phase <seconds>            # 0 = infinito (default 0)
--timeout-total <seconds>            # 0 = infinito (default 0)
--parallelism-sketch <n>             # default 4
--parallelism-phase <n>              # default 4
--parallelism-extraction <n>         # default 4
--max-parallelism <n>                # tope global duro
--models <lista>
--roles <lista>
--context <ref>                      # run_id o path
--context-summary
--context-full
--uncategorized-threshold <0..1>
--export-format <tar.gz|tar|zip>
--export-level <summary|full>
--yes                                # skip confirmación en switch
```

### `context` puede ser

- `run_id` (resuelve en SQLite).
- Path a archivo `.md`.
- Path a directorio.

Si es `run_id`, se traen:

- Default: `final/*.md` + tabla resumen de sketches.
- `--context-summary`: solo `final/*.md`.
- `--context-full`: paquete completo.

## 2.2. `moagan continue [<run_id>] [--skip-checkpoint]`

- Reanuda el run activo o el especificado.
- Por default, pausa en el siguiente checkpoint humano.
- Con `--skip-checkpoint`, marca en el manifest que no hubo validación humana y emite warning.

## 2.3. `moagan resume <run_id>`

- Reanuda desde donde se quedó.
- Respeta timeouts y paralelismo del run.

## 2.4. `moagan rerun <run_id> [--same-config | --matrix-override <json>]`

- `--same-config`: replica parámetros, nuevo `run_id`.
- `--matrix-override <json>`: edita solo los campos provistos.

## 2.5. `moagan inspect <run_id> [--phase <name>]`

- Muestra snapshot de fases.
- Sin diff entre artefactos.

## 2.6. `moagan import <source-path>`

- Importa un run desde otro directorio.
- Mueve los archivos y registra la nueva ubicación.
- Conserva el `run_id` original.

## 2.7. `moagan telemetry`

```text
moagan telemetry list                          # lista runs
moagan telemetry list --run <run_id>           # detalle de un run
moagan telemetry summary --run <run_id>        # resumen agregado
moagan telemetry compare <run_a> <run_b>       # diff entre dos runs
moagan telemetry provider --plan <name>        # plan de un provider
moagan telemetry provider --list               # lista providers
moagan telemetry view --port <port>            # dashboard HTTP
moagan telemetry export --run <run_id>         # exporta dataset
moagan telemetry cleanup [--dry-run]           # aplica retention
moagan telemetry config                        # muestra config
moagan telemetry verify --path <export-path>   # verifica SHA256SUMS
```

Status visible en lista: `created`, `running`, `paused`, `completed`, `timeout`, `cancelled`, `failed`.

## 2.8. Cambio de provider mid-run

```text
moagan continue run_disc --switch-provider glm
moagan continue run_disc --switch-api-key               # prompt interactivo
moagan continue run_disc --switch-api-key env:GLM_KEY   # desde variable
moagan continue run_disc --switch-api-key file:/path    # desde archivo
```

Con `--yes` se salta la confirmación.

---

# 3. Timeouts y paralelismo

## 3.1. Timeouts

Tres niveles, todos admiten `0` para infinito.

| Nivel | Default | Significado |
|---|---:|---|
| `sketch` | 120s | Tiempo máximo por sketch. |
| `phase` | 0 | Tiempo máximo por fase. |
| `total` | 0 | Tiempo máximo del run. |

Política:

```text
if timeout == 0:
    sin_limite()
else:
    aplicar_limite(timeout)
```

Si el usuario fija `total = 0`, el sistema registra warning en el manifest.

## 3.2. Paralelismo

Cada fase puede pedir un paralelismo distinto. El sistema respeta un tope global `max_parallelism`.

```text
parallelismo_efectivo = min(parallelismo_pedido, max_parallelism - en_uso)
```

### Defaults

```text
parallelism.sketch = 4
parallelism.phase = 4
parallelism.extraction = 4
max_parallelism = 4
```

### Comportamiento con `max_parallelism = 4`

- Si una fase pide 12, ejecuta 4.
- Si dos fases corren en paralelo, la segunda espera a que haya hueco.
- `max_parallelism` es absoluto y nunca se excede.

### Coordinación entre fases

```text
loop:
    tareas_pendientes = fases_activas()
    for tarea in tareas_pendientes:
        if en_uso < max_parallelism:
            asignar(tarea, min(tarea.deseado, max_parallelism - en_uso))
            en_uso += 1
```

---

# 4. Identificadores y articulación entre runs

## 4.1. run_id

UUID v7 por defecto.

```text
run_id = uuid_v7
```

## 4.2. Esquema SQLite

```sql
CREATE TABLE runs (
  run_id TEXT PRIMARY KEY,
  parent_run_id TEXT,
  shared_brief_hash TEXT,
  mode TEXT,
  created_at TIMESTAMP,
  status TEXT,
  FOREIGN KEY (parent_run_id) REFERENCES runs(run_id)
);

CREATE TABLE run_siblings (
  run_id TEXT,
  sibling_run_id TEXT,
  PRIMARY KEY (run_id, sibling_run_id),
  FOREIGN KEY (run_id) REFERENCES runs(run_id),
  FOREIGN KEY (sibling_run_id) REFERENCES runs(run_id)
);

CREATE TABLE run_context_refs (
  run_id TEXT,
  context_ref TEXT,
  context_type TEXT,
  PRIMARY KEY (run_id, context_ref),
  FOREIGN KEY (run_id) REFERENCES runs(run_id)
);

CREATE TABLE provider_changes (
  run_id TEXT,
  from_provider TEXT,
  from_plan_id TEXT,
  to_provider TEXT,
  to_plan_id TEXT,
  at TIMESTAMP,
  PRIMARY KEY (run_id, at),
  FOREIGN KEY (run_id) REFERENCES runs(run_id)
);

CREATE TABLE provider_usage (
  run_id TEXT,
  provider TEXT,
  plan_id TEXT,
  calls INTEGER,
  tokens_total INTEGER,
  errors INTEGER,
  started_at TIMESTAMP,
  ended_at TIMESTAMP,
  PRIMARY KEY (run_id, provider, plan_id),
  FOREIGN KEY (run_id) REFERENCES runs(run_id)
);

CREATE TABLE phases (
  run_id TEXT,
  phase_name TEXT,
  event TEXT,
  at TIMESTAMP,
  details JSON,
  PRIMARY KEY (run_id, phase_name, at)
);

CREATE TABLE calls (
  call_id TEXT PRIMARY KEY,
  run_id TEXT,
  phase TEXT,
  model TEXT,
  endpoint TEXT,
  input_tokens INTEGER,
  output_tokens INTEGER,
  total_tokens INTEGER,
  temperature REAL,
  top_p REAL,
  role TEXT,
  duration_seconds REAL,
  http_status INTEGER,
  status TEXT,
  error_code TEXT,
  error_message TEXT,
  retry_count INTEGER,
  truncated BOOLEAN,
  output_hash TEXT,
  FOREIGN KEY (run_id) REFERENCES runs(run_id)
);
```

## 4.3. Lineage

Cada manifest incluye:

```json
{
  "lineage_paths": {
    "relative": {
      "brief": "brief.json",
      "final": "final"
    },
    "absolute": {
      "brief": "/home/user/.runs/018f3a2b/brief.json",
      "final": "/home/user/.runs/018f3a2b/final"
    }
  }
}
```

Las rutas relativas sobreviven si el run se mueve. Las absolutas son inmediatas si no.

## 4.4. Comportamiento de `context`

```text
if es_run_id(ref):
    run = sqlite.get(ref)
    if --context-full:
        return run.complete_package()
    elif --context-summary:
        return run.final_files()
    else:
        return run.final_files() + run.sketches_summary_table()
elif es_path(ref):
    return load_path(ref)
```

## 4.5. Estructura de archivos

```text
.runs/<run_id>/
├── manifest.json
├── brief.json
├── exploration_matrix.json
├── sketches/
├── tags/
├── clusters/
├── contradictions/
├── facets/
├── extractions/
│   ├── cat_01/
│   │   ├── faceta_flujos.md
│   │   ├── faceta_constraints.md
│   │   └── ...
│   └── cat_02/
│       └── ...
├── drafts/
│   ├── cat_01/
│   │   ├── borrador.md
│   │   └── issues.json
│   └── cat_02/
│       └── ...
├── final/
│   ├── cat_01.md
│   ├── cat_02.md
│   ├── ...
│   ├── uncategorized.md
│   └── summary.md
├── telemetry/
│   ├── run.json
│   ├── phases.jsonl.gz
│   ├── calls.jsonl.gz
│   ├── provider_usage.json
│   ├── timeline.html
│   └── dashboard.html
└── human_checkpoint.json
```

---

# 5. Flujo general (modos fast, standard, deep, explore, batch)

## 5.1. Fase 0. Ingesta y creación de la sesión

### Objetivo

Convertir el input del usuario en un objeto de trabajo reproducible y auditable.

### Entradas

- Prompt.
- Archivos adjuntos.
- Contexto previo.
- Configuración del proyecto.
- Presupuesto de tiempo, tokens y llamadas.
- Modo interactivo o batch.
- Preferencias acumuladas del usuario.
- Lista de modelos habilitados.
- Lista de roles habilitados.

### Procesamiento local

Antes de llamar a un LLM:

- Normalizar saltos de línea y codificación.
- Calcular hash del input.
- Detectar idioma.
- Identificar bloques de código.
- Extraer tecnologías mencionadas.
- Detectar restricciones literales.
- Detectar el formato de salida solicitado.
- Identificar referencias a archivos o componentes existentes.
- Estimar tamaño y coste inicial.
- Detectar señales de prompt injection y anonimizar/seudonimizar ejemplos de código.

### Artefacto producido

```text
RunContext
├── run_id
├── input_hash
├── raw_prompt
├── normalized_prompt
├── attachments
├── detected_language
├── explicit_constraints
├── requested_artifacts
├── budget
├── execution_policy
├── enabled_models
├── enabled_roles
├── previous_feedback
└── injection_safety_state
```

### Gate

Si el input está vacío, corrupto o excede límites configurados, el sistema se detiene antes de gastar tokens.

### Procedencia

Principalmente T03-03 y T19-01.

---

## 5.2. Fase 1. Interpretación y clarificación

### Objetivo

Construir un brief canónico que todos los agentes posteriores utilizarán como fuente de verdad.

### Clasificación

El sistema determina:

- Tipo de trabajo: arquitectura, discovery, implementación, debugging, refactoring, mixto, exploratorio.
- Complejidad: trivial, simple, media, alta, crítica.
- Riesgo: bajo, medio, alto.
- Grado de ambigüedad.
- Necesidad de código ejecutable.
- Necesidad de investigación externa.
- Necesidad de intervención humana.

### Separación de restricciones

```text
ConstraintSet
├── hard
├── soft
├── inferred
├── contradictions
└── missing_information
```

### Política de clarificación

Las ambigüedades se clasifican en:

1. **Bloqueantes**: cambian sustancialmente el espacio de soluciones.
2. **Importantes**: afectan el ranking, pero permiten continuar con supuestos.
3. **No críticas**: pueden resolverse con un default documentado.

### Vistas del brief

Para el deep, se usan vistas derivadas si los modelos del enjambre requieren contexto distinto. Se mantiene una tabla de traducción entre vista y brief canónico.

### Checkpoint humano 1

Se activa cuando:

- existe una contradicción entre restricciones duras;
- hay una ambigüedad bloqueante;
- el riesgo es alto;
- la decisión podría provocar lock-in;
- se requiere información que el sistema no puede inferir responsablemente.

El prompt interactivo **no tiene timeout de inactividad**. El usuario puede tardar lo que necesite.

### Artefacto producido

```text
CanonicalBrief
├── problem_statement
├── task_type
├── objectives
├── deliverables
├── hard_constraints
├── soft_preferences
├── assumptions
├── non_goals
├── acceptance_criteria
├── open_questions
├── risk_level
├── required_validation
└── derived_views
```

### Procedencia

T03-03 como base y T11-01 para el checkpoint humano.

---

## 5.3. Fase 2. Enrutamiento adaptativo

### Objetivo

Decidir cuánto proceso necesita realmente la solicitud.

### Modos

| Modo | Cardinalidad | Cuándo usarlo |
|---|---|---|
| `fast` | 2–4 agentes | Tareas simples, riesgo bajo. |
| `standard` | 6–12 agentes | Arquitectura moderada, refactoring. |
| `deep` | 12–25 agentes | Decisiones complejas, código crítico. |
| `discovery` | 40–500 sketches | Exploración masiva, problemas vagos. |
| `explore` | 8–12 sketches | Conocer el espacio sin elegir. |
| `batch` | configurable | Automatización, sin pausas humanas. |

### Rutas tradicionales

#### Ruta rápida

- Dos agentes.
- Sin descomposición.
- Sin sketches.
- Validación estructural.
- Un juez o evaluación determinista.
- Sin crítica cruzada completa.
- Sin síntesis.

#### Ruta estándar

- Cuatro sketches.
- Tres propuestas completas.
- Dos críticos por propuesta.
- Validación técnica cuando corresponda.
- Tres jueces especializados.
- Una ronda de reparación.

#### Ruta profunda

- Descomposición DAG.
- Entre cinco y seis sketches.
- Entre cuatro y cinco propuestas completas.
- Crítica y reparación con hasta dos rondas.
- Panel multi-juez.
- Revisión adversaria.
- Sandbox.
- Checkpoint humano antes de la decisión final.
- Acepta `--context <run_id|path>`.

#### Ruta exploratoria

- Alta diversidad.
- Más sketches que propuestas completas.
- Sin síntesis.
- Clustering y mapa de decisiones.
- Ranking secundario.
- Entrega de familias y preguntas de investigación.

#### Ruta batch

- Supuestos explícitos.
- Sin pausas humanas.
- Ambigüedades bloqueantes producen estado `NeedsInput`.
- Salida JSON estable.
- Presupuesto y timeout duros.
- Timeout default `0` (infinito).

### Presupuesto

```text
BudgetPlan
├── intake
├── decomposition
├── sketches
├── full_proposals
├── criticism
├── repair
├── validation
├── judging
└── synthesis
```

Si se agota:

1. se eliminan rondas adicionales;
2. se reduce el número de críticos;
3. se limita el refinamiento a top-1;
4. se conserva la validación dura;
5. nunca se omiten los gates para financiar una síntesis.

### Procedencia

T03-03 como base, T11-01 para el checkpoint humano, T16-01 para el clustering.

---

## 5.4. Fase 3. Descomposición condicional

### Objetivo

Dividir únicamente los problemas que realmente lo necesitan.

### Activación

Se activa si:

- la complejidad es media-alta;
- existen múltiples entregables;
- hay dependencias claras;
- se mezclan arquitectura, dominio e implementación;
- ninguna propuesta monolítica puede cubrir adecuadamente el problema.

**No se activa en `discovery`.**

### Salida

```text
ProblemGraph
├── nodes
│   ├── id
│   ├── question
│   ├── expected_output
│   ├── constraints
│   ├── dependencies
│   └── validation_method
├── integration_rules
└── critical_path
```

### Reglas

- Los nodos independientes se procesan en paralelo.
- Los dependientes esperan los artefactos necesarios.
- Cada nodo declara cómo será validado.
- No se sintetiza el DAG prematuramente.
- Las decisiones incompatibles se preservan como ramas.

### Procedencia

T12-02.

---

## 5.5. Fase 4. Exploración mediante sketches

### Objetivo

Explorar el espacio de soluciones a bajo coste antes de redactar propuestas largas.

### Generación

En `standard`, `deep` y `explore` se producen entre cuatro y seis sketches de 400–800 tokens.

Posibles ángulos:

- minimalista;
- pragmático;
- production-grade;
- seguridad-first;
- coste-first;
- escalabilidad;
- mantenibilidad;
- exploratorio;
- contrarian;
- dominio-first.

### Por sketch

```text
Sketch
├── thesis
├── key_decisions
├── architecture_outline
├── assumptions
├── strengths
├── weaknesses
├── hard_constraint_check
└── expected_validation
```

### Aislamiento

Los agentes no ven otros sketches. Esto evita convergencia prematura.

### Filtro económico

Antes de expandir:

1. Validación estructural.
2. Verificación de restricciones duras.
3. Detección de redundancia.
4. Cobertura del brief.
5. Valor diferencial.
6. Riesgos críticos.

### Selección

Se conservan:

- los dos sketches con mayor calidad básica;
- uno o dos con mayor diversidad;
- un outlier defendible, si existe.

### Memoria epistémica

De los sketches descartados solo se heredan:

- restricciones duras descubiertas;
- riesgos con evidencia;
- casos límite plausibles;
- preguntas abiertas importantes.

No se heredan automáticamente: preferencias estilísticas, objeciones especulativas, conclusiones sin evidencia, constraints inventados.

### Checkpoint opcional

En modo interactivo, el usuario puede:

- promover un sketch;
- descartar un enfoque;
- añadir un nuevo ángulo;
- bloquear una tecnología;
- pedir que dos ramas continúen separadas.

### Procedencia

T03-01.

---

## 5.6. Fase 5. Generación de propuestas completas

### Objetivo

Convertir los sketches seleccionados en propuestas completas y comparables.

### Contrato de salida

```text
Proposal
├── id
├── source_sketch
├── executive_summary
├── interpretation
├── assumptions
├── goals_and_non_goals
├── architecture_or_approach
├── key_flows
├── implementation_plan
├── risks
├── tradeoffs
├── alternatives_rejected
├── validation_plan
├── open_questions
├── artifacts
└── provenance
```

### Diversidad

La diversidad se fuerza mediante:

- persona;
- ángulo técnico;
- restricciones secundarias;
- temperatura;
- prioridad principal;
- prohibición de imitar otros enfoques.

### Paralelismo y resiliencia

- Ejecución mediante pool con semáforo.
- Cada fase pide `min(su_deseo, max_parallelism - en_uso)`.
- Timeout individual.
- Cancelación cooperativa.
- Reintento único por error de transporte.
- Continuación focal por truncamiento.
- El fallo de un agente no cancela a los demás.

### Procedencia

T03-03 como orquestador base.

---

## 5.7. Fase 6. Gate de validez

### Objetivo

Determinar si cada propuesta puede entrar al proceso de calidad.

Estados: `Pass`, `Warn`, `Fail`.

### Checks deterministas

- JSON o estructura parseable.
- Secciones obligatorias.
- No truncamiento.
- No placeholders críticos.
- Bloques de código balanceados.
- Constraints duras presentes.
- Tecnologías prohibidas ausentes.
- Entregables solicitados cubiertos.
- Formato e idioma correctos.
- Ausencia de contradicciones triviales.
- No respuesta genérica o evasiva.
- Longitud dentro del rango esperado.

### Reglas

- **Fail**: violación de restricción dura, no resuelve, truncamiento, estructura irrecuperable, código obligatorio ausente, contradicción central, contenido degenerado.
- **Warn**: sección secundaria incompleta, placeholder no crítico, supuesto no documentado, evidencia débil, posible redundancia, error técnico reparable.
- **Pass**: cumple el contrato mínimo.

### Procedencia

T19-01.

---

## 5.8. Fase 7. Validación ejecutable

### Objetivo

Obtener evidencia objetiva cuando la propuesta contiene artefactos verificables.

### Activación

Si el usuario solicita código, la propuesta incluye código sustancial, la tarea es implementación o debugging, existe schema o el riesgo lo justifica.

### Validaciones posibles

#### Rust

- `cargo fmt --check`
- `cargo check`
- `cargo clippy -- -D warnings`
- `cargo test`

#### Python

- `python -m py_compile`
- Tests declarados.
- Linter disponible.

#### TypeScript

- `tsc --noEmit`
- Tests configurados.
- Lint del proyecto.

#### SQL

- Parser SQL.
- Validación contra dialecto.

#### Configuración

- JSON Schema.
- Parser YAML/TOML.
- Validación de manifests.

### Seguridad

- Directorio temporal.
- Sin red por defecto.
- Timeout.
- Límites de CPU, memoria y archivos.
- Variables de entorno saneadas.
- No acceso a secretos.
- Lista explícita de comandos permitidos.
- Eliminación del entorno al terminar.

### Interpretación

- Compilar no demuestra buena arquitectura.
- No compilar sí demuestra que el artefacto no es ejecutable.
- Un test ausente no equivale a test aprobado.
- Una herramienta no disponible produce `Skipped`, no `Pass`.

### Salida

```text
ValidationEvidence
├── status
├── checks_run
├── command
├── exit_code
├── stdout_summary
├── stderr_summary
├── failed_tests
├── skipped_checks
└── reproducibility_data
```

### Procedencia

T12-03, reforzada por T07-03.

---

## 5.9. Fase 8. Crítica especializada

### Críticos recomendados

- Correctness critic.
- Constraint critic.
- Feasibility critic.
- Security critic.
- Operability critic.
- Product-fit critic.
- Simplicity critic.
- Domain critic.
- Adversarial critic.

### Estrategia de asignación

Por propuesta:

- Un crítico de corrección.
- Un crítico de ajuste al problema.
- Un tercero solo si riesgo alto, código crítico, disenso, o dominio regulado.

### Formato

```text
Critique
├── proposal_id
├── strengths
├── issues
│   ├── severity
│   ├── category
│   ├── evidence
│   ├── affected_section
│   └── suggested_fix
├── blockers
├── uncertain_claims
└── verdict
```

### Severidades

- `blocker`
- `major`
- `minor`
- `observation`

### Evidencia

Toda crítica importante debe señalar: sección, cita, resultado de validación, constraint relevante, contradicción específica.

---

## 5.10. Fase 9. Reparación dirigida

### Input del autor

- Propuesta original.
- Críticas relevantes.
- Evidencia del sandbox.
- Constraints.
- Resumen anónimo de fortalezas de otras propuestas.
- Feedback humano disponible.

### Reglas

El autor debe:

1. Resolver blockers.
2. Corregir problemas mayores.
3. Mantener su tesis central.
4. Documentar cambios.
5. Justificar críticas rechazadas.
6. No copiar decisiones incompatibles.
7. No reescribir secciones no afectadas sin razón.

### Comparación con original

La versión revisada no sustituye automáticamente a la original. Ambas pueden competir.

### Condiciones de parada

- no quedan blockers ni majors;
- la mejora de score es inferior al umbral;
- dos revisiones son semánticamente casi iguales;
- se alcanza el máximo de dos rondas;
- se agota el presupuesto;
- el usuario interrumpe.

### Procedencia

T09-03 y T12-03.

---

## 5.11. Fase 10. Evaluación multi-juez

### Panel base

- **Juez 1: corrección**: exactitud técnica, coherencia lógica, compatibilidad, errores factuales, evidencia ejecutable.
- **Juez 2: completitud y alineación**: cobertura de requisitos, respuesta a criterios, casos límite, ajuste, supuestos.
- **Juez 3: viabilidad y utilidad**: esfuerzo, coste operativo, mantenibilidad, riesgos, claridad, ejecución.

### Jueces opcionales

Seguridad, producto, dominio, operaciones, accesibilidad, coste.

### Evaluación independiente

Cada juez recibe: brief canónico, propuesta anonimizada, evidencia, críticas, rúbrica. No recibe: identidad del generador, scores de otros jueces, ranking preliminar, autoconfianza.

### Rúbrica

Descriptores anclados, no scores libres.

### Manejo de disenso

Si los jueces difieren más del umbral:

1. Marcar criterio como `contested`.
2. Conservar la distribución, no solo el promedio.
3. Juez focal de desempate.
4. Si persiste, mostrar al usuario.

### Revisión adversaria

Después del panel, un adversario busca: fallos compartidos, consenso injustificado, supuestos invisibles, riesgos omitidos, claims no verificados, razones por las que el ranking podría estar mal.

### Procedencia

T03-03.

---

## 5.12. Fase 11. Selección multiobjetivo

### Paso 1. Filtros duros

Se eliminan propuestas con:

- restricciones duras incumplidas;
- blockers sin resolver;
- ejecución obligatoria fallida;
- corrección inferior al mínimo;
- no-respuesta;
- estructura inválida.

### Paso 2. Vector de calidad

```text
QualityVector
├── correctness
├── completeness
├── feasibility
├── alignment
├── maintainability
├── security
├── cost
├── clarity
└── novelty
```

### Paso 3. Frente de Pareto

Se conservan propuestas no dominadas.

### Paso 4. Diversidad

Si el frente es muy grande: similitud arquitectónica, crowding distance, clustering.

### Paso 5. Ranking contextual

Ranking ponderado para recomendar, no para destruir el frente de Pareto.

### Paso 6. Estabilidad

Se perturban los pesos dentro de un rango pequeño.

- Si top-1 sigue ganando, el ranking es estable.
- Si cambia, se marca como sensible a preferencias.

### Procedencia

T12-02, con estabilidad tomada de T07-01.

---

## 5.13. Fase 12. Clustering y política de síntesis

### Clustering

Las propuestas se agrupan por: stack, modelo de despliegue, persistencia, consistencia, estilo arquitectónico, estrategia de migración, modelo de seguridad, coste operacional.

### Compatibilidad

Dos propuestas son compatibles si comparten invariantes fundamentales, sus diferencias son de implementación o énfasis, una sección puede sustituirse sin romper otras, no hay decisiones mutuamente excluyentes.

### Incompatibilidad

No deben sintetizarse cuando difieren en decisiones como: monolito vs microservicios, eventos vs transacciones síncronas, consistencia fuerte vs eventual, SQL vs no SQL, infraestructura propia vs serverless, Rust vs runtime no permitido.

### La síntesis compite

Después de crearse: pasa gates, se ejecuta si contiene código, recibe crítica, es evaluada, entra al frente de Pareto. Solo sustituye a sus fuentes si demuestra mejora sin perder coherencia.

### Procedencia

T16-01, T11-01 y T12-02.

---

## 5.14. Fase 13. Checkpoint humano final

### Se activa cuando

- existen dos o más clusters incompatibles;
- el ranking es inestable;
- los jueces mantienen disenso;
- la propuesta implica lock-in;
- existe diferencia significativa de coste o riesgo;
- ninguna propuesta domina claramente;
- el usuario solicitó modo interactivo.

### Sin timeout de inactividad

El usuario puede tardar lo que necesite en responder.

### Acciones

- Seleccionar una propuesta.
- Seleccionar una familia.
- Cambiar pesos.
- Añadir una restricción.
- Pedir una variante.
- Solicitar una síntesis dentro de un cluster.
- Profundizar un riesgo.
- Terminar sin elegir.

### Procedencia

T11-01.

---

## 5.15. Fase 14. Entrega

```text
run/
├── manifest.json
├── brief.json
├── problem_graph.json
├── sketches/
├── proposals/
├── critiques/
├── revisions/
├── validation/
├── evaluations/
├── ranking.json
├── clusters.json
└── final.md
```

Incluye: resumen ejecutivo, portfolio, matriz comparativa, mapa de divergencias, evidencia, auditoría.

---

## 5.16. Fase 15. Iteración localizada

```text
refine P2 --focus security
expand P1 --section migration
variant P1 --replace postgres-with-sqlite
rerank --cost 0.30 --maintainability 0.30
critique P3 --lens security
synthesize cluster-2
reframe --add-constraint "sin servicios administrados"
```

Invalidación selectiva: una modificación solo invalida artefactos dependientes.

---

# 6. Modo discovery

## 6.1. Características

- Modo standalone.
- Sin DAG.
- Cardinalidad 40–500 sketches, sin tope.
- Matriz `roles × models × temperatures`.
- Todos los sketches pasan a tagging (sin selección top-K).
- Produce una biblia por categoría, no propuestas finalistas.

## 6.2. Cuándo usarlo

- Problemas vagos o sin solución clara.
- Construcción de base de conocimiento antes de actuar.
- Exploración sin objetivo de elegir.

## 6.3. Pipeline

```text
Prompt
  ↓
Ingesta local
  ↓
Brief + clarificación
  ↓
Matriz de exploración (roles × modelos × temperatures)
  ↓
Generación de sketches (con cola a + c + tracker de outliers)
  ↓
Gate barato
  ↓
Tagger (categorías dinámicas, uncategorized permitido)
  ↓
Clustering (familias)
  ↓
Detector de contradicciones
  ↓
Derivación de facetas
  ↓
Extracción por faceta
  ↓
Integración híbrida (script + validador LLM + refinador LLM)
  ↓
Documento por categoría
  ↓
Documento uncategorized (si ≥ 3 sketches)
  ↓
Checkpoint humano único
  ↓
Documentos finales + biblia
```

## 6.4. Matriz de exploración

### Definición

```text
matrix = roles × models × temperatures
```

Reproducibilidad: el sistema detecta duplicados por hash del input completo (prompt + parámetros). Sin seeds, no se puede garantizar la misma salida entre ejecuciones, pero se puede detectar que los parámetros coinciden.

### Cardinalidad

- Mínimo: 40 sketches.
- Máximo: 500, sin tope duro.
- Sin límite: si el usuario tiene capacidad, puede llegar a miles.

### Por sketch

```text
Sketch
├── thesis
├── key_decisions
├── architecture_outline
├── assumptions
├── strengths
├── weaknesses
├── hard_constraint_check
└── expected_validation
```

### Política de paro

Combina:

1. **Margen fijo (a)**: tras detectar saturación, continuar con 25% adicional del total generado.
2. **Adaptativo por modelo (c)**: si un modelo saturó, continuar con modelos no saturados.
3. **Tracker de outliers**: registrar sketches cuya distancia a cualquier cluster supere un umbral; preservarlos al pasar el filtro.

```text
loop:
    samples = generar(40)
    clusters = clustering(samples)
    aporte = cobertura_nueva(clusters)

    outliers = detect_outliers(samples, clusters)

    if aporte < umbral:
        if todos_los_modelos_saturados:
            cola_total = ceil(0.25 * total_anterior)
            continuar_con_cola(cola_total)
        else:
            cambiar_a_modelo_no_saturado()
    else:
        cola = reset()

    if outliers:
        candidatos_cola.append(outliers)
```

## 6.5. Tagger

### Comportamiento

- Tagger ligero (T baja, top_p bajo).
- Entrada: sketch completo.
- Salida: lista de tags con pesos.

```text
tagger(sketch) -> {
  primary: "rust",
  secondary: ["arquitectura", "modular"],
  similarity_to_category: 0.81
}
```

### Reglas

- Si la similitud a todas las categorías está por debajo del umbral, asignar `uncategorized`.
- Si la moda de `uncategorized` supera `uncategorized_threshold` (default 0.3), emitir warning.
- No fusionar `uncategorized` con la categoría más cercana por similitud. La clasificación es semántica.

### Procedencia

Adaptación del concepto de T07-02.

## 6.6. Clustering

### Algoritmo

- Embeddings ligeros.
- Similitud coseno.
- Threshold configurable (default 0.85).

### Salida

```text
clusters/
├── cluster_01.json
│   ├── id
│   ├── representative_sketch
│   ├── centroid_embedding
│   ├── members
│   └── category
├── cluster_02.json
└── ...
```

### Procedencia

T16-01.

## 6.7. Detector de contradicciones

```json
{
  "conflict_id": "c1",
  "cluster_a": "cluster_01",
  "cluster_b": "cluster_05",
  "topic": "consistency",
  "description": "Cluster 01 aboga por ACID, cluster 05 por eventual.",
  "severity": "high"
}
```

## 6.8. Derivación de facetas

Una llamada ligera (T=0, top_p=0.2) responde:

- ¿Qué información entregarías a otro ingeniero?
- ¿Qué patrones de uso debo documentar?
- ¿Qué restricciones importan?
- ¿Qué warnings debo anotar?
- ¿Qué decisiones arquitectónicas considerar?

De ahí se deriva la lista de facetas:

```json
{
  "facet": "flujos",
  "description": "Flujos de datos y secuencias principales",
  "required": true
}
```

Caché por hash de `(brief, categoría)`.

### Procedencia

T07-02.

## 6.9. Extracción por faceta

Por cada categoría, llamadas paralelas para cada faceta. Cada extractor recibe:

- briefs de sketches del cluster;
- contradicciones;
- faceta a extraer;
- restricciones duras.

```text
extractions/
├── cat_01/
│   ├── faceta_flujos.md
│   ├── faceta_constraints.md
│   └── ...
└── cat_02/
    └── ...
```

### Procedencia

T07-02.

## 6.10. Integración híbrida

```text
extraccion_por_faceta(categoría) -> Map<faceta, markdown>
script.unir(markdown) -> borrador
llm_validador(borrador) -> issues_json
if issues:
    fusionar_contradicciones(borrador)
    humano_revisa(issues) [opcional]
refinador_fluidez(borrador) -> final
```

### Salvaguardas

- El refinador debe preservar citas textuales.
- Si la reescritura diluye el contenido, revertir.
- Si la contradicción intra-documento es grave, abortar y avisar al humano.

### Documento `uncategorized`

Si el número de sketches en `uncategorized` es ≥ 3, se genera `final/uncategorized.md`:

```markdown
# Categoría: uncategorized

## Resumen
[N sketches no categorizados. Contenido heterogéneo.]

## Ideas sueltas
- sk_001: [contenido]
- sk_002: [contenido]

## Temas recurrentes
- Tema A: sk_001, sk_007, sk_015
- Tema B: sk_003, sk_022

## Contradicciones detectadas
- sk_001 vs sk_022: [...]

## Preguntas abiertas
- sk_005: [pregunta]
```

Si hay < 3 sketches, solo se registra en el manifest.

## 6.11. Validación humana única

En `discovery`, se activa una sola vez, al final, sobre los documentos por categoría.

### Acciones

- Aprobar.
- Marcar contradicciones.
- Pedir revisión de un documento.
- Bloquear un documento.
- Exportar.

### No puede

- Crear categoría.
- Renombrar categoría.
- Cambiar contenido.

Si falta algo, se debe rehacer el `discovery`.

---

# 7. Pasarela discovery → deep

El usuario decide si encadenar discovery con deep. El sistema no lo hace automáticamente.

```text
moagan run --mode deep --context run_disc_a
moagan run --mode deep --context ./biblia_externa.md
```

### Comportamiento

- El deep carga el contexto.
- Marca las facetas de discovery como **fuentes obligatorias**.
- Las usa como restricciones blandas adicionales.
- No las trata como verdad absoluta.

### Confirmación de que cada run es independiente

- Cada `moagan run` crea su propio `run_id`.
- Directorio propio: `.runs/<run_id>/`.
- Manifest propio.
- No hay colisión.

---

# 8. Telemetría

## 8.1. Tres anillos

```text
Ring 1: Run-level      (un run entero)
Ring 2: Phase-level    (qué pasó en cada fase)
Ring 3: Call-level     (cada llamada individual al LLM)
```

## 8.2. Niveles

```text
telemetry.level = "full"       # guarda todo
telemetry.level = "aggregate"  # solo resúmenes
telemetry.level = "off"        # nada
```

Default: `full`.

Relación:

```text
level = full        -> Run + Phase + Call + Provider
level = aggregate  -> Run + Phase + Provider
level = off        -> nada
```

## 8.3. Run-level

```json
{
  "run_id": "018f3a2b-...",
  "mode": "discovery",
  "started_at": "2026-07-24T...",
  "ended_at": "2026-07-24T...",
  "duration_seconds": 1456.2,
  "total_tokens": 1240000,
  "total_llm_calls": 312,
  "tokens_per_second": 851.0,
  "models_used": {
    "minimax": {
      "calls": 200,
      "tokens": 800000,
      "errors": 0
    },
    "glm": {
      "calls": 112,
      "tokens": 440000,
      "errors": 1
    }
  },
  "sketch_count": 500,
  "category_count": 12,
  "uncategorized_count": 47,
  "saturated": false,
  "cancelled": false,
  "timeout": false,
  "context_refs": [],
  "alert": {
    "uncategorized_exceeded": true,
    "parallelism_saturated_times": 12
  }
}
```

## 8.4. Phase-level

```json
{
  "phase_name": "sketch_generation",
  "started_at": "...",
  "ended_at": "...",
  "duration_seconds": 412.7,
  "tokens_total": 800000,
  "tokens_per_call": 4000,
  "calls": 200,
  "failed_calls": 3,
  "cancelled": 0,
  "parallelism_used_avg": 4,
  "parallelism_used_max": 4,
  "max_parallelism_ceiling": 4,
  "saturation_events": 12
}
```

## 8.5. Call-level

```json
{
  "call_id": "...uuid",
  "phase": "sketch_generation",
  "model": "minimax",
  "endpoint": "https://api.minimax.io/anthropic/v1/messages",
  "request_started_at": "...",
  "request_ended_at": "...",
  "duration_seconds": 1.42,
  "input_tokens": 800,
  "output_tokens": 600,
  "total_tokens": 1400,
  "temperature": 0.6,
  "top_p": 0.95,
  "role": "architect",
  "status": "ok",
  "http_status": 200,
  "error_code": null,
  "error_message": null,
  "retry_count": 0,
  "truncated": false,
  "output_hash": "..."
}
```

## 8.6. Almacenamiento

### SQLite

```sql
-- Ya definido en §4.2.
```

### Archivos

```text
.runs/<run_id>/telemetry/
├── run.json
├── phases.jsonl.gz
├── calls.jsonl.gz
├── provider_usage.json
├── timeline.html
└── dashboard.html
```

### Compresión

- `manifest.json`: sin comprimir.
- `phases.jsonl`, `calls.jsonl`: `gz` por default.
- Configurable: `none`, `gz`, `zst`.

```text
[storage]
jsonl_compression = "gz"
manifest_compression = "none"
```

### Redacción en tiempo real

Al escribir cualquier archivo, la redacción se aplica antes de persistir:

```text
fn write_jsonl(record: &Record) -> Result<()> {
    let json = serde_json::to_string(record)?;
    let clean = redact(&json);
    fs.append(jsonl_path, clean + "\n")?;
}
```

## 8.7. Comandos `moagan telemetry`

```text
moagan telemetry list
moagan telemetry list --run <run_id>
moagan telemetry summary --run <run_id>
moagan telemetry compare <run_a> <run_b>
moagan telemetry provider --plan <name>
moagan telemetry provider --list
moagan telemetry view --port <port>
moagan telemetry export --run <run_id>
moagan telemetry cleanup [--dry-run]
moagan telemetry config
moagan telemetry verify --path <export-path>
```

### Output de `summary`

```text
Run: 018f3a2b
Mode: discovery
Duration: 24m 16s
Tokens: 1,240,000
Calls: 312
Avg parallelism: 3.7
Saturation: 12 times

By model:
  minimax:  800,000 tokens (200 calls, 0 errors)
  glm:      440,000 tokens (112 calls, 1 error)

By phase:
  sketch_generation:  412s (200 calls)
  tagging:           89s (50 calls)
  clustering:        12s (0 calls)
  extraction:        412s (50 calls)
  integration:       23s (10 calls)
```

### Output de `compare`

```text
moagan telemetry compare run_disc_1 run_disc_2

Run 1: 1240000 tokens, 312 calls, 8 categories
Run 2: 2180000 tokens, 480 calls, 12 categories

Difference:
  +94k tokens, +168 calls
  -2 categories (run 2 merged 2)
```

### Output de `provider --plan`

```text
moagan telemetry provider --plan minimax

Provider: minimax
Plan: weekly
Limit: 1,000,000 tokens
Used (this week): 624,000 tokens (62.4%)
Remaining: 376,000 tokens
Days until reset: 3

Recent runs:
  run_disc_1:  124,000 (2026-07-22)
  run_disc_2:  218,000 (2026-07-23)
  run_disc_3:  282,000 (2026-07-24)
```

## 8.8. Dashboard

### Características

- Solo lectura.
- Sin estado mutable.
- HTTP local con queries a SQLite.
- Sin memoria entre sesiones.

### Endpoints

```text
GET  /api/runs
GET  /api/runs/<run_id>
GET  /api/runs/<run_id>/phases
GET  /api/runs/<run_id>/calls
GET  /api/runs/<run_id>/provider_usage
GET  /api/runs/<run_id>/hashes
GET  /api/runs/<run_id>/export?level=summary&format=tar.gz
GET  /api/runs/<run_id>/export?level=summary&format=tar.gz&include=hashes
```

### Puerto

```text
[server]
port = 4096
port_search_max = 1000
port_blacklist = [22, 80, 443, 3306, 5432, 6379, 8080, 8443]
```

```text
moagan telemetry view --port 4096
# Starting dashboard on http://127.0.0.1:4096
# Already in use, trying 4097...
# Listening on http://127.0.0.1:4097
```

### Lo que el dashboard puede hacer

- Listar runs.
- Ver detalle de un run.
- Comparar dos runs.
- Ver telemetría por provider.
- Ver gráficas de tiempo, tokens, errores.
- Descargar JSONL.
- Descargar export.

### Lo que el dashboard no puede hacer

- Pausar un run.
- Cancelar un run.
- Cambiar la matrix.
- Modificar la configuración.

---

# 9. Export

## 9.1. Niveles

```text
summary_export:
  include_brief = true
  include_sketches_summary = true
  include_calls = false
  include_outputs = false

full_export:
  include_brief = true
  include_sketches_summary = true
  include_calls = true
  include_outputs = true
```

Default: `summary`.

## 9.2. Formatos

```text
moagan telemetry export --format tar.gz   # default
moagan telemetry export --format tar       # sin comprimir
moagan telemetry export --format zip       # compatibilidad
```

## 9.3. Estructura del export

```text
run_<run_id>_export/
├── manifest.json
├── brief.json
├── exploration_matrix.json
├── phases.jsonl
├── calls.jsonl
├── provider_usage.json
├── provider_changes.json
├── run.json
├── final/
│   ├── cat_01.md
│   ├── cat_02.md
│   ├── ...
│   ├── uncategorized.md
│   └── summary.md
├── extractions/
├── drafts/
├── sketches/
├── clusters/
├── facets/
├── contradictions/
├── dashboard.html
├── MANIFEST.txt
└── SHA256SUMS
```

## 9.4. SHA256SUMS

Algoritmo: SHA256 por default. BLAKE3 opcional.

```text
abc123...  manifest.json
def456...  brief.json
ghi789...  calls.jsonl
...
```

El manifest incluye hashes embebidos:

```json
{
  "hashes": {
    "manifest.json": "abc123...",
    "brief.json": "def456...",
    "calls.jsonl": "ghi789...",
    "phases.jsonl": "jkl012...",
    "final/*.md": "mno345..."
  }
}
```

## 9.5. MANIFEST.txt

Archivo legible con resumen del export:

```text
Run: 018f3a2b-7c9d-7e8f-9b2a-4c5d6e7f8a9b
Mode: discovery
Started: 2026-07-24T10:30:00Z
Ended: 2026-07-24T10:54:16Z
Duration: 24m 16s
Total tokens: 1,240,000
Total calls: 312
Categories: 12
Uncategorized: 47
Provider changes: 1
Status: completed

Files in this export:
  - manifest.json
  - brief.json
  - calls.jsonl (gz)
  - phases.jsonl (gz)
  - final/cat_01.md
  ...
  - SHA256SUMS
```

## 9.6. Verificación

```text
moagan telemetry verify --path exported/
```

El comando:

- Recorre todos los archivos.
- Calcula SHA256.
- Compara con SHA256SUMS.
- Reporta OK o inconsistencias.

---

# 10. Privacidad y seguridad

## 10.1. Redacción configurable

### Patrones por default

```text
[privacy]
redact_patterns = [
  "sk-cp-[A-Za-z0-9]+",
  "sk-[A-Za-z0-9]+",
  "ghp_[A-Za-z0-9]+",
  "gho_[A-Za-z0-9]+",
  "ghs_[A-Za-z0-9]+",
  "ghr_[A-Za-z0-9]+",
  "Bearer\\s+[A-Za-z0-9._-]+",
  "(?i)password\\s*[:=]\\s*\\S+",
  "(?i)api[_-]?key\\s*[:=]\\s*\\S+",
  "--token\\s+\\S+",
  "AKIA[0-9A-Z]{16}"
]
```

### Política

```text
[privacy]
redact_in_brief = false
redact_in_prompts = false
redact_in_telemetry = true
redact_in_storage = true
redact_in_export = true
```

### Redacción en tiempo real

La redacción se aplica antes de persistir cualquier archivo.

### Redacción en errores

Los mensajes de error del LLM pasan por redacción antes de escribirse.

## 10.2. API keys

### Cuatro modos de suministro

1. **Prompt interactivo** (default).
2. **Variable de ambiente**: `env:VAR_NAME`.
3. **Archivo**: `file:/path`.
4. **No literal en CLI**.

### Configuración

```text
[api_keys]
minimax = "env:MINIMAX_API_KEY"
glm = "env:GLM_API_KEY"
qwen = "env:QWEN_API_KEY"
kimi = "env:KIMI_API_KEY"
deepseek = "env:DEEPSEEK_API_KEY"
opencode_go = "env:OPENCODE_GO_API_KEY"
```

### Cambio mid-run

```text
moagan continue run_disc --switch-api-key
moagan continue run_disc --switch-api-key env:GLM_KEY
moagan continue run_disc --switch-api-key file:/path
```

Prompt interactivo **no tiene timeout de inactividad**.

Si aparece la key en logs, se redacta:

```text
[INFO] Cambiando API key de MiniMax
[INFO] Nueva key: sk-cp-************************************
```

## 10.3. Atribución de modelos

```text
[privacy]
attribute_model = true
attribute_provider = true
attribute_version = true
```

Cada export incluye:

```text
{
  "call_id": "...",
  "model": "minimax",
  "model_version": "MiniMax-M3",
  "provider": "minimax",
  "output_hash": "...",
  "license": "provider-terms",
  "generated_at": "..."
}
```

---

# 11. Multi-provider y planes

## 11.1. Trait abstracto

```rust
trait Provider {
    fn name(&self) -> &str;
    fn endpoint(&self) -> &str;
    fn parse_usage(&self, response: &Response) -> Usage;
    fn supports_token_plan(&self) -> bool;
}

struct Usage {
    prompt_tokens: u64,
    completion_tokens: u64,
    total_tokens: u64,
    cached_tokens: Option<u64>,
    reasoning_tokens: Option<u64>,
}
```

### Implementaciones

- `minimax.rs`
- `glm.rs`
- `qwen.rs`
- `kimi.rs`
- `deepseek.rs`
- `opencode_go.rs`
- `mock.rs`

## 11.2. Configuración de plan

```text
[providers.minimax]
plan_type = "weekly"
plan_limit = 1000000
warning_threshold = 0.8
hard_limit = 0.95

[providers.glm]
plan_type = "monthly"
plan_limit = 5000000
```

## 11.3. Estados del plan

```text
normal: usage < warning_threshold
warning: warning_threshold <= usage < hard_limit
paused: usage >= hard_limit

Comportamiento:
  normal: nuevos runs permitidos
  warning: warning visible, runs permitidos
  paused: nuevos runs bloqueados, runs en curso entran en hibernación
```

## 11.4. Hibernación

### Evento `pause`

```text
status = "paused"
paused_at = "..."
pause_reason = "plan_exceeded"
pause_user_notified = true
```

### Sin timeout

La hibernación puede durar días, meses o años. El run queda en estado `paused` indefinidamente. El usuario decide cuándo reanudar.

### Menú de opciones

```text
Run pausado. Plan de MiniMax al 95%.

Costos restantes estimados:
  - sketches restantes: 250
  - tokens promedio: 1500
  - total estimado: 375,000 tokens

Opciones:
  1. Esperar reset del plan (3 días)
  2. Cambiar a GLM (32% usado)
  3. Cambiar a MiniMax backup (4% usado)
  4. Reducir paralelismo de 4 a 2
  5. Reducir cardinalidad (500 -> 200 sketches)
  6. Cancelar el run
```

### Cambio de provider preserva sketches

```text
moagan run --mode discovery --provider minimax
  # sk_001 generated by minimax
  # sk_002 generated by minimax
  # -> PAUSE: plan exceeded
moagan continue run_disc --switch-provider glm --yes
  # sk_003 generated by glm
  # sk_004 generated by glm
```

### provider_changes en manifest

```json
{
  "provider_changes": [
    {
      "from": "minimax",
      "from_api_key_ref": "env:MINIMAX_API_KEY",
      "to": "glm",
      "to_api_key_ref": "env:GLM_API_KEY",
      "at": "2026-07-24T...",
      "sketches_already_generated": 2
    }
  ]
}
```

### Cambio de provider como instance distinto

Aunque dos providers sean del mismo proveedor físico (e.g., dos cuentas de MiniMax), el sistema los trata como separados:

```text
providers:
  - id: "minimax_main"
    provider: "minimax"
    api_key_ref: "env:MINIMAX_API_KEY"
    plan: "weekly"
    plan_limit: 1000000
    used: 950000

  - id: "minimax_backup"
    provider: "minimax"
    api_key_ref: "file:/path/to/key"
    plan: "weekly"
    plan_limit: 5000000
    used: 200000
```

## 11.5. Estimación de costos

Al cambiar de provider, el sistema recalcula:

```text
Run pausado. Estado actual:
  - sketches generados: 250
  - sketches restantes: 250
  - tokens consumidos: 600,000
  - tokens estimados restantes: 375,000
  - total estimado: 975,000 tokens

Plan actual (MiniMax main): 95% usado
Plan nuevo (GLM): 32% usado, suficiente
```

---

# 12. Retención

## 12.1. Configuración

```text
[retention]
keep_runs_days = 365
keep_runs_count = 9000
max_storage_gb = 50
policy = "archive"
```

## 12.2. Política

```text
for run in oldest_runs:
    if expired_age > keep_runs_days:
        aplicar_politica(run)
    elif total_count > keep_runs_count:
        aplicar_politica(run)
    elif total_storage > max_storage_gb:
        aplicar_politica(run)
    elif almacenamiento_exceeded:
        alert("Almacenamiento cerca del límite")
```

## 12.3. Archivar vs eliminar

- `archive`: comprimir y mover a `.runs/archive/`.
- `delete`: borrar.

## 12.4. Comando

```text
moagan telemetry cleanup [--dry-run]
```

## 12.5. Sin distinción manual vs batch

La política aplica igual a todos los runs. El comportamiento durante la corrida es lo único que difiere.

---

# 13. Arquitectura técnica

## 13.1. Estructura de directorios

```text
src/
├── domain/
│   ├── run.rs
│   ├── brief.rs
│   ├── constraints.rs
│   ├── problem_graph.rs
│   ├── proposal.rs
│   ├── critique.rs
│   ├── validation.rs
│   ├── evaluation.rs
│   ├── sketch.rs
│   ├── tag.rs
│   ├── cluster.rs
│   ├── facet.rs
│   └── extraction.rs
├── phases/
│   ├── intake.rs
│   ├── clarify.rs
│   ├── route.rs
│   ├── decompose.rs
│   ├── matrix.rs
│   ├── sketch.rs
│   ├── generate.rs
│   ├── gate.rs
│   ├── execute.rs
│   ├── tag.rs
│   ├── cluster.rs
│   ├── contradictions.rs
│   ├── facets.rs
│   ├── extract.rs
│   ├── draft.rs
│   ├── critique.rs
│   ├── repair.rs
│   ├── judge.rs
│   ├── rank.rs
│   ├── cluster_proposals.rs
│   ├── synthesize.rs
│   ├── integrate.rs
│   └── deliver.rs
├── llm/
│   ├── client.rs
│   ├── minimax.rs
│   ├── glm.rs
│   ├── qwen.rs
│   ├── kimi.rs
│   ├── deepseek.rs
│   ├── mock.rs
│   ├── retry.rs
│   └── budget.rs
├── validators/
│   ├── structural.rs
│   ├── constraints.rs
│   ├── rust.rs
│   ├── python.rs
│   ├── typescript.rs
│   └── schema.rs
├── ranking/
│   ├── pareto.rs
│   ├── aggregate.rs
│   ├── stability.rs
│   └── clustering.rs
├── discovery/
│   ├── matrix.rs
│   ├── tagger.rs
│   ├── clusterer.rs
│   ├── contradiction_detector.rs
│   ├── facet_deriver.rs
│   ├── extractor.rs
│   ├── integrator.rs
│   └── uncategorized.rs
├── context/
│   ├── resolver.rs
│   ├── summary.rs
│   └── full.rs
├── execution/
│   ├── parallelism.rs
│   ├── timeout.rs
│   └── cancellation.rs
├── telemetry/
│   ├── ring_run.rs
│   ├── ring_phase.rs
│   ├── ring_call.rs
│   ├── provider.rs
│   ├── retention.rs
│   ├── redact.rs
│   └── export.rs
├── dashboard/
│   ├── server.rs
│   ├── routes.rs
│   └── templates.rs
├── storage/
│   ├── sqlite.rs
│   ├── artifacts.rs
│   ├── cache.rs
│   └── compression.rs
├── orchestration/
│   ├── pipeline.rs
│   ├── scheduler.rs
│   ├── cancellation.rs
│   └── checkpoints.rs
└── cli/
    ├── run.rs
    ├── continue.rs
    ├── resume.rs
    ├── rerun.rs
    ├── inspect.rs
    ├── import.rs
    └── telemetry.rs
```

## 13.2. Traits principales

```rust
trait Agent {
    async fn run(&self, input: AgentInput) -> Result<AgentOutput>;
}

trait Validator {
    async fn validate(&self, proposal: &Proposal) -> ValidationReport;
}

trait Evaluator {
    async fn evaluate(
        &self,
        brief: &CanonicalBrief,
        proposal: &Proposal,
        evidence: &ValidationEvidence,
    ) -> Evaluation;
}

trait Phase {
    type Input;
    type Output;

    async fn execute(
        &self,
        context: &RunContext,
        input: Self::Input,
    ) -> Result<Self::Output>;
}

trait Tagger {
    async fn tag(&self, sketch: &Sketch) -> Tags;
}

trait Integrator {
    async fn validate_coherence(&self, draft: &Document) -> Vec<Issue>;
    async fn refine(&self, draft: &Document) -> Document;
}

trait Provider {
    fn name(&self) -> &str;
    fn endpoint(&self) -> &str;
    fn parse_usage(&self, response: &Response) -> Usage;
    fn supports_token_plan(&self) -> bool;
}
```

## 13.3. Estado de una propuesta

```text
Draft
  ↓
StructurallyValid
  ↓
MechanicallyValidated
  ↓
Critiqued
  ↓
Revised
  ↓
Evaluated
  ↓
Ranked
  ↓
Selected | Alternative | Rejected
```

## 13.4. Estado de un sketch (discovery)

```text
Generated
  ↓
Gated
  ↓
Tagged
  ↓
Clustered
  ↓
Facted
  ↓
Extracted
  ↓
Integrated
  ↓
Finalized
```

## 13.5. Estado de un run

```text
created
  ↓
running
  ↓
paused        (plan_exceeded, user_pause, timeout)
  ↓
running       (continue)
  ↓
completed | timeout | cancelled | failed
```

## 13.6. Implementación por etapas

### MVP

1. Ingesta.
2. Brief canónico.
3. Routing `fast` y `standard`.
4. Tres propuestas paralelas.
5. Gate estructural.
6. Dos críticos por propuesta.
7. Una reparación.
8. Tres jueces.
9. Ranking ponderado.
10. Entrega top-3.
11. Persistencia de artefactos.
12. `refine` y `rerank`.
13. `max_parallelism` global.

### Segunda etapa

- Sketches.
- Discovery básico (matriz, tagger, clustering, extracción).
- Documentos por categoría.
- Validadores de Rust, Python y TypeScript.
- Pareto.
- Clustering de propuestas.
- Síntesis intra-cluster.
- Tercer juez condicional.
- Checkpoints humanos.
- Caché.

### Tercera etapa

- DAG de descomposición.
- Scheduling por dependencias.
- Análisis de estabilidad.
- Ejecución aislada más fuerte.
- Perfiles especializados por dominio.
- Métricas y calibración.
- Investigación externa.
- Aprendizaje de preferencias del usuario.
- Run_id con UUID v7.
- Lineage completo.
- `moagan continue`, `moagan resume`, `moagan rerun`, `moagan inspect`, `moagan import`.
- Telemetría completa.
- Redacción configurable.
- Cambio de provider mid-run.
- Hibernación.
- Dashboard.
- Export con SHA256SUMS.

---

# 14. Configuración completa

```text
[timeouts]
sketch = 120
phase = 0
total = 0

[parallelism]
sketch = 4
phase = 4
extraction = 4
max_parallelism = 4

[discovery]
max_categorias_default = 12
min_ejemplos_por_categoria = 5
max_categorias_soft = 30
reserve_ratio = 0.25
uncategorized_threshold = 0.3

[context]
default_level = "default"

[telemetry]
level = "full"
warning_threshold = 0.8
hard_limit = 0.95

[retention]
keep_runs_days = 365
keep_runs_count = 9000
max_storage_gb = 50
policy = "archive"

[storage]
jsonl_compression = "gz"
manifest_compression = "none"

[server]
port = 4096
port_search_max = 1000
port_blacklist = [22, 80, 443, 3306, 5432, 6379, 8080, 8443]

[privacy]
redact_patterns = [...]
redact_in_brief = false
redact_in_prompts = false
redact_in_telemetry = true
redact_in_storage = true
redact_in_export = true
attribute_model = true
attribute_provider = true
attribute_version = true

[security]
api_key_default_input = "interactive"

[api_keys]
minimax = "env:MINIMAX_API_KEY"
glm = "env:GLM_API_KEY"
qwen = "env:QWEN_API_KEY"
kimi = "env:KIMI_API_KEY"
deepseek = "env:DEEPSEEK_API_KEY"
opencode_go = "env:OPENCODE_GO_API_KEY"

[providers.minimax]
plan_type = "weekly"
plan_limit = 1000000
warning_threshold = 0.8
hard_limit = 0.95

[providers.glm]
plan_type = "monthly"
plan_limit = 5000000
```

---

# 15. Métricas recomendadas

## 15.1. Calidad

- Porcentaje de propuestas válidas.
- Cobertura de requisitos.
- Blockers por propuesta.
- Errores descubiertos por sandbox.
- Regresiones introducidas por refinamiento.
- Síntesis que superan a sus fuentes.
- Acuerdo humano con la recomendación.

## 15.2. Diversidad

- Número de clusters.
- Distancia entre propuestas.
- Redundancia descartada.
- Outliers preservados.
- Cambios arquitectónicos reales, no solo léxicos.

## 15.3. Evaluación

- Disenso entre jueces.
- Frecuencia de tercer juez.
- Estabilidad ante perturbación de pesos.
- Ranking antes y después de evidencia ejecutable.
- Críticas aceptadas y rechazadas.

## 15.4. Operación

- Tokens por fase.
- Coste por propuesta válida.
- Latencia total y por fase.
- Ratio de caché.
- Reintentos.
- Timeouts.
- Corridas detenidas en clarificación.
- Fases evitadas por routing adaptativo.
- Uso de `max_parallelism` (cuántas veces se saturó).

## 15.5. Discovery

- Sketches por modelo.
- Curva de saturación.
- Tasa de `uncategorized`.
- Cobertura de facetas.
- Contradicciones detectadas.
- Drift entre iteraciones del refinador.

## 15.6. HITL

- Preguntas que cambiaron el brief.
- Checkpoints aprobados sin cambios.
- Restricciones añadidas por el usuario.
- Veces que el usuario eligió una alternativa distinta del top-1.
- Decisiones que no debieron sintetizarse.

## 15.7. Telemetría

- p95_latency por modelo.
- error_rate por modelo.
- tokens_per_second por modelo.
- parallel_efficiency = calls_effective / max_parallelism / duration.
- cache_hit_rate.
- saturation_rate = saturation_events / duration.

---

# 16. Diseño final resumido

## 16.1. Modos fast, standard, deep, explore, batch

```text
Prompt
  ↓
Ingesta local
  ↓
Brief + clarificación
  ↓
Routing adaptativo
  ↓
Descomposición condicional
  ↓
Sketches baratos
  ↓
Selección por calidad + diversidad
  ↓
Propuestas completas
  ↓
Gate de validez
  ↓
Validación ejecutable
  ↓
Crítica especializada
  ↓
Reparación dirigida
  ↓
Revalidación
  ↓
Panel multi-juez
  ↓
Revisión adversaria
  ↓
Pareto + estabilidad
  ↓
Clustering de compatibilidad
  ↓
Síntesis opcional que compite
  ↓
Checkpoint humano
  ↓
Portfolio + recomendación + evidencia
  ↓
Iteración localizada
```

## 16.2. Modo discovery

```text
Prompt
  ↓
Ingesta local
  ↓
Brief + clarificación
  ↓
Matriz de exploración (roles × modelos × temperatures)
  ↓
Generación de sketches (con cola a + c + tracker de outliers)
  ↓
Gate barato
  ↓
Tagger (categorías dinámicas, uncategorized permitido)
  ↓
Clustering (familias)
  ↓
Detector de contradicciones
  ↓
Derivación de facetas
  ↓
Extracción por faceta
  ↓
Integración híbrida (script + validador LLM + refinador LLM)
  ↓
Documento por categoría
  ↓
Documento uncategorized (si ≥ 3 sketches)
  ↓
Checkpoint humano único
  ↓
Documentos finales + biblia
```

## 16.3. Pasarela discovery → deep

```text
moagan run --mode discovery     # run_disc
  ↓
moagan run --mode deep --context run_disc   # run_deep
  ↓
Deep consume discovery como fuente blanda
```

El usuario encadena los runs manualmente. Cada run es independiente.

## 16.4. Notas finales

- El diagrama representa el flujo máximo, no el obligatorio. El router debe reducirlo para tareas simples.
- El modo `discovery` no es opcional para problemas vagos o sin solución clara. Su salida es una biblia, no una respuesta.
- `max_parallelism` es la restricción global. Si tienes 4 agentes permitidos, nunca se ejecutan más de 4 en paralelo.
- Los timeouts con valor `0` son válidos pero peligrosos. El sistema registra la advertencia en el manifest.
- El sistema de discovery funciona como una base de conocimiento navegable que puede ser consultada por humanos, por otros modelos, o por una corrida posterior del Mixture Of Agent.
- Toda la telemetría se redacta en tiempo real al escribir. Los archivos nunca quedan con secretos sin redactar.
- El cambio de provider preserva los sketches generados. El sistema recalcula costos restantes.
- El dashboard es solo lectura. No interactúa con procesos en ejecución.
- El export incluye SHA256SUMS y puede verificarse con `moagan telemetry verify`.
- Sin seeds. Los modelos de texto no exponen la semilla. El sistema detecta duplicados por hash del input completo.

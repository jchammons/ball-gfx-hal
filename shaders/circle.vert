#version 450

layout (set = 0, binding = 0) uniform Ubo {
  vec2 scale;
} globals;

layout (location = 0) in vec2 inPos;

layout (push_constant) uniform PushConstant {
  float radius;
  vec2 center;
  vec4 color;
} push_constants;

layout (location = 0) out vec2 outPos;

void main() {
  outPos = inPos * push_constants.radius;
  gl_Position = vec4(globals.scale * (inPos * push_constants.radius + push_constants.center), 0.0, 1.0);
}

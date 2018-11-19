#version 450
// #extension GL_OES_standard_derivatives : enable

layout (set = 0, binding = 0) uniform Ubo {
  vec2 scale;
} globals;

layout (location = 0) in vec2 inPos;

layout (push_constant) uniform PushConstant {
  float radius;
  vec2 center;
  vec4 color;
} push_constants;

layout (location = 0) out vec4 outColor;

void main() {
  float radius_sq = push_constants.radius * push_constants.radius;
  float len_sq = dot(inPos, inPos);
  if(len_sq > radius_sq) {
    discard;
  }
  float delta = fwidth(len_sq);
  float alpha = 1.0 - smoothstep(radius_sq - delta, radius_sq, len_sq);
  // float alpha = clamp((push_constants.radius - globals.pixel_size - len) / globals.pixel_size, 0.0, 1.0);
  outColor = vec4(push_constants.color.rgb, push_constants.color.a * alpha);
}
